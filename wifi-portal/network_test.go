package main

import (
	"bytes"
	"context"
	"errors"
	"net/http"
	"net/url"
	"os"
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
	var allowed atomic.Bool
	allowed.Store(true)
	var reserves, releases, connects atomic.Int32
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

	var releaseReply, connectReply atomic.Pointer[dbus.Error]
	var rejectConnect atomic.Bool
	methods := map[string]any{
		"Scan": func() *dbus.Error { return nil },
		"Reserve": func(token string) (string, *dbus.Error) {
			if token != "" {
				return "", dbus.NewError("org.freedesktop.DBus.Error.InvalidArgs", nil)
			}
			reserves.Add(1)
			allowed.Store(true)
			return controllerToken, nil
		},
		"Authorized": func(token string) (bool, *dbus.Error) { return token == controllerToken && allowed.Load(), nil },
		"Connect": func(raw []byte, security, password, uuid, token string) (uint64, *dbus.Error) {
			if !bytes.Equal(raw, ssid) || security != "sae" || password != "test" || uuid != "" || token != controllerToken {
				return 0, dbus.NewError("org.freedesktop.DBus.Error.InvalidArgs", nil)
			}
			connects.Add(1)
			if rejectConnect.Load() {
				return 0, dbus.NewError("org.freedesktop.DBus.Error.Failed", []any{"network-busy"})
			}
			if !allowed.Load() {
				return 0, dbus.NewError("org.freedesktop.DBus.Error.AccessDenied", []any{"invalid-reservation"})
			}
			state := *current.Load()
			state.AttemptID++
			state.Phase = "connecting"
			current.Store(&state)
			allowed.Store(false)
			return state.AttemptID, connectReply.Load()
		},
		"Release": func(token string) *dbus.Error {
			if token != controllerToken {
				return dbus.NewError("org.freedesktop.DBus.Error.InvalidArgs", nil)
			}
			releases.Add(1)
			return releaseReply.Load()
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
	token, err := client.reserve(ctx)
	if err != nil || token != controllerToken {
		t.Fatalf("Reserve: %v", err)
	}
	if allowed, err := client.authorized(ctx, token); err != nil || !allowed {
		t.Fatalf("Authorized: %v, %v", allowed, err)
	}
	if err := client.scan(ctx); err != nil {
		t.Fatalf("Scan: %v", err)
	}
	if id, err := client.connect(ctx, candidate{ssid: ssid, security: "sae", password: "test"}, token); err != nil || id != 13 {
		t.Fatalf("Connect: %v, %v", id, err)
	}
	if err := client.release(ctx, token); err != nil {
		t.Fatalf("Release: %v", err)
	}

	for _, tc := range []struct {
		name, errorName, reason string
		canRetry                bool
	}{
		{"consumed", "Failed", "invalid-reservation", true},
		{"consumed on pinned controller", "AccessDenied", "invalid-reservation", true},
		{"bus policy denied release", "AccessDenied", "Rejected send message", false},
		{"other failure", "Failed", "network-busy", false},
		{"same text in an unknown error", "InvalidArgs", "invalid-reservation", false},
		{"reason only contains the code", "Failed", "unexpected invalid-reservation response", false},
	} {
		t.Run(tc.name, func(t *testing.T) {
			current.Store(&status{Mode: "hotspot", Address: testHost, AttemptID: 12, Phase: "idle"})
			allowed.Store(true)
			releaseReply.Store(nil)
			p := &portal{hotspotAddress: testHotspot, network: client}
			connect := url.Values{"action": {"connect"}, "network": {"sae:ff78"}, "password": {"test"}}
			if w := response(p, request("POST", connect, nil)); w.Code != http.StatusAccepted || len(w.Result().Cookies()) != 0 {
				t.Fatalf("cookieless connect: HTTP %d", w.Code)
			}
			beforeReserve, beforeRelease, beforeConnect := reserves.Load(), releases.Load(), connects.Load()
			if w := response(p, request("POST", connect, nil)); w.Code != http.StatusConflict || releases.Load() != beforeRelease || connects.Load() != beforeConnect {
				t.Fatalf("active attempt released or resubmitted: HTTP %d", w.Code)
			}
			current.Store(&status{Mode: "hotspot", Address: testHost, AttemptID: 13, Phase: "failed"})
			releaseReply.Store(dbus.NewError("org.freedesktop.DBus.Error."+tc.errorName, []any{tc.reason}))
			w := response(p, request("POST", connect, nil))
			if !tc.canRetry {
				if w.Code != http.StatusConflict || reserves.Load() != beforeReserve || connects.Load() != beforeConnect || p.token != controllerToken {
					t.Fatalf("unexpected error discarded reservation: HTTP %d", w.Code)
				}
				return
			}
			if w.Code != http.StatusAccepted || reserves.Load() != beforeReserve+1 || releases.Load() != beforeRelease+1 || connects.Load() != beforeConnect+1 || len(w.Result().Cookies()) != 0 {
				t.Fatalf("consumed token not replaced on manual retry: HTTP %d", w.Code)
			}
		})
	}

	t.Run("explicit rejection allows later manual request", func(t *testing.T) {
		current.Store(&status{Mode: "hotspot", Address: testHost, AttemptID: 20, Phase: "failed"})
		rejectConnect.Store(true)
		p := &portal{hotspotAddress: testHotspot, network: client}
		form := url.Values{"action": {"connect"}, "network": {"sae:ff78"}, "password": {"test"}}
		beforeConnect, beforeRelease := connects.Load(), releases.Load()
		if w := response(p, request("POST", form, nil)); w.Code != http.StatusServiceUnavailable || connects.Load() != beforeConnect+1 || releases.Load() != beforeRelease {
			t.Fatalf("rejected dispatch: HTTP %d", w.Code)
		}
		rejectConnect.Store(false)
		if w := response(p, request("POST", form, nil)); w.Code != http.StatusAccepted || connects.Load() != beforeConnect+2 || releases.Load() != beforeRelease {
			t.Fatalf("later manual request blocked: HTTP %d", w.Code)
		}
	})

	t.Run("lost connect reply keeps active reservation", func(t *testing.T) {
		current.Store(&status{Mode: "hotspot", Address: testHost, AttemptID: 20, Phase: "idle"})
		connectReply.Store(dbus.MakeFailedError(errors.New("lost reply")))
		p := &portal{hotspotAddress: testHotspot, network: client}
		form := url.Values{"action": {"connect"}, "network": {"sae:ff78"}, "password": {"test"}}
		beforeConnect, beforeRelease := connects.Load(), releases.Load()
		if w := response(p, request("POST", form, nil)); w.Code != http.StatusServiceUnavailable || connects.Load() != beforeConnect+1 || releases.Load() != beforeRelease {
			t.Fatalf("lost reply: HTTP %d", w.Code)
		}
		if w := response(p, request("POST", form, nil)); w.Code != http.StatusConflict || connects.Load() != beforeConnect+1 || releases.Load() != beforeRelease {
			t.Fatalf("active lost reply released or replayed: HTTP %d", w.Code)
		}
	})
}
