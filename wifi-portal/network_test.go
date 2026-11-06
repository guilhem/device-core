package main

import (
	"bytes"
	"context"
	"errors"
	"net/http"
	"net/url"
	"os"
	"strconv"
	"sync/atomic"
	"testing"

	"github.com/godbus/dbus/v5"
)

func TestDeviceCoreWire(t *testing.T) {
	if os.Getenv("DEVICE_CORE_PORTAL_TEST_DBUS") != "1" {
		t.Skip("requires a private dbus-run-session")
	}
	server, err := dbus.ConnectSessionBus()
	if err != nil {
		t.Fatal(err)
	}
	defer server.Close()
	if reply, err := server.RequestName(busName, dbus.NameFlagDoNotQueue); err != nil || reply != dbus.RequestNameReplyPrimaryOwner {
		t.Fatalf("request bus name: %v, %v", reply, err)
	}
	ssid := []byte{0xff, 'x'}
	var current atomic.Pointer[status]
	current.Store(&status{Mode: "hotspot", Address: testHost, SSID: ssid, AttemptID: 12, Phase: "failed"})
	var connects atomic.Int32
	properties := map[string]dbus.Variant{
		"Status":   dbus.MakeVariant(status{Mode: "hotspot", Address: testHost, SSID: ssid, AttemptID: 12, Phase: "failed"}),
		"Networks": dbus.MakeVariant([]accessPoint{{SSID: ssid, Strength: 92, Security: "sae"}}),
		"Profiles": dbus.MakeVariant([]profile{{UUID: "saved", SSID: ssid}}),
	}
	for name, signature := range map[string]string{
		"Status": "(ssbsaystss)", "Networks": "a(ayys)", "Profiles": "a(say)",
	} {
		if got := properties[name].Signature().String(); got != signature {
			t.Fatalf("%s signature: got %s, want %s", name, got, signature)
		}
	}
	if err := server.ExportMethodTable(map[string]any{"Get": func(iface, property string) (dbus.Variant, *dbus.Error) {
		value, exists := properties[property]
		if iface != networkInterface || !exists {
			return dbus.Variant{}, dbus.NewError("org.freedesktop.DBus.Error.InvalidArgs", nil)
		}
		if property == "Status" {
			value = dbus.MakeVariant(*current.Load())
		}
		return value, nil
	}}, networkPath, "org.freedesktop.DBus.Properties"); err != nil {
		t.Fatal(err)
	}

	var connectReply atomic.Pointer[dbus.Error]
	var active atomic.Bool
	methods := map[string]any{
		"Scan": func() *dbus.Error { return nil },
		"ConnectHotspot": func(raw []byte, security, password, uuid string) (uint64, *dbus.Error) {
			connects.Add(1)
			if !bytes.Equal(raw, ssid) || security != "sae" || password != "test" || uuid != "" {
				return 0, dbus.NewError("org.freedesktop.DBus.Error.InvalidArgs", nil)
			}
			if !active.CompareAndSwap(false, true) {
				return 0, dbus.NewError("org.freedesktop.DBus.Error.Failed", []any{"network-busy"})
			}
			state := *current.Load()
			state.AttemptID++
			state.Phase = "connecting"
			current.Store(&state)
			return state.AttemptID, connectReply.Load()
		},
	}

	if err := server.ExportMethodTable(methods, networkPath, networkInterface); err != nil {
		t.Fatal(err)
	}

	bus, err := dbus.ConnectSessionBus()
	if err != nil {
		t.Fatal(err)
	}
	defer bus.Close()
	client := deviceCore{bus.Object(busName, networkPath)}
	ctx, cancel := context.WithTimeout(t.Context(), callTimeout)
	defer cancel()
	if got, err := client.status(ctx); err != nil || !got.hotspot(testHotspot) || got.AttemptID != 12 || !bytes.Equal(got.SSID, ssid) {
		t.Fatalf("Status: %#v, %v", got, err)
	}
	if got, err := client.networks(ctx); err != nil || len(got) != 1 || got[0].Strength != 92 || !bytes.Equal(got[0].SSID, ssid) {
		t.Fatalf("Networks: %#v, %v", got, err)
	}
	if got, err := client.profiles(ctx); err != nil || len(got) != 1 || got[0].UUID != "saved" || !bytes.Equal(got[0].SSID, ssid) {
		t.Fatalf("Profiles: %#v, %v", got, err)
	}
	if err := client.scan(ctx); err != nil {
		t.Fatalf("Scan: %v", err)
	}
	if id, err := client.connect(ctx, candidate{ssid: ssid, security: "sae", password: "test"}); err != nil || id != 13 {
		t.Fatalf("ConnectHotspot: %v, %v", id, err)
	}

	for _, lostReply := range []bool{false, true} {
		t.Run("lost reply="+strconv.FormatBool(lostReply), func(t *testing.T) {
			active.Store(false)
			connectReply.Store(nil)
			if lostReply {
				connectReply.Store(dbus.MakeFailedError(errors.New("lost reply")))
			}
			p := &portal{hotspotAddress: testHotspot, network: client}
			form := url.Values{"action": {"connect"}, "network": {"sae:ff78"}, "password": {"test"}}
			r := request("POST", form, nil)
			r.Header.Set("Origin", "null")
			canceled, stop := context.WithCancel(r.Context())
			stop()
			before := connects.Load()
			w := response(p, r.WithContext(canceled))
			want := http.StatusAccepted
			if lostReply {
				want = http.StatusServiceUnavailable
			}
			if w.Code != want || connects.Load() != before+1 || !active.Load() || len(w.Result().Cookies()) != 0 {
				t.Fatalf("detached direct call: HTTP %d, calls=%d", w.Code, connects.Load()-before)
			}
			if w := response(p, request("POST", form, nil)); w.Code != http.StatusServiceUnavailable || connects.Load() != before+2 {
				t.Fatalf("controller admission bypassed: HTTP %d calls=%d", w.Code, connects.Load()-before)
			}
			active.Store(false)
			connectReply.Store(nil)
			if w := response(p, request("POST", form, nil)); w.Code != http.StatusAccepted || connects.Load() != before+3 {
				t.Fatalf("later manual call blocked: HTTP %d calls=%d", w.Code, connects.Load()-before)
			}
		})
	}
}
