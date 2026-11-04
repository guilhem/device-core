package main

import (
	"bytes"
	"context"
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
		return value, nil
	}}, networkPath, "org.freedesktop.DBus.Properties"); err != nil {
		t.Fatal(err)
	}

	var releaseReply atomic.Pointer[dbus.Error]
	methods := map[string]any{
		"Scan": func() *dbus.Error { return nil },
		"Reserve": func(token string) (string, *dbus.Error) {
			if token != "" {
				return "", dbus.NewError("org.freedesktop.DBus.Error.InvalidArgs", nil)
			}
			return controllerToken, nil
		},
		"Authorized": func(token string) (bool, *dbus.Error) { return token == controllerToken, nil },
		"Connect": func(raw []byte, security, password, uuid, token string) (uint64, *dbus.Error) {
			if !bytes.Equal(raw, ssid) || security != "sae" || password != "test" || uuid != "" || token != controllerToken {
				return 0, dbus.NewError("org.freedesktop.DBus.Error.InvalidArgs", nil)
			}
			return 13, nil
		},
		"Release": func(token string) *dbus.Error {
			if token != controllerToken {
				return dbus.NewError("org.freedesktop.DBus.Error.InvalidArgs", nil)
			}
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
			p := &portal{hotspotAddress: testHotspot, network: client}
			reserve := url.Values{"action": {"reserve"}}
			release := url.Values{"action": {"release"}}
			w := response(p, request("POST", reserve, nil))
			cookies := w.Result().Cookies()
			if w.Code != http.StatusSeeOther || len(cookies) != 1 {
				t.Fatalf("initial reservation: HTTP %d", w.Code)
			}
			cookie := cookies[0]
			connect := url.Values{"action": {"connect"}, "network": {"sae:ff78"}, "password": {"test"}}
			if w := response(p, request("POST", connect, cookie)); w.Code != http.StatusAccepted {
				t.Fatalf("connect: HTTP %d", w.Code)
			}

			releaseReply.Store(dbus.NewError("org.freedesktop.DBus.Error."+tc.errorName, []any{tc.reason}))
			w = response(p, request("POST", release, cookie))
			cookies = w.Result().Cookies()
			if !tc.canRetry {
				if w.Code != http.StatusConflict || len(cookies) != 0 {
					t.Fatalf("unexpected error cleared browser session: HTTP %d", w.Code)
				}
				if w := response(p, request("POST", reserve, nil)); w.Code != http.StatusConflict {
					t.Fatalf("unexpected error allowed a new reservation: HTTP %d", w.Code)
				}
				return
			}
			if w.Code != http.StatusSeeOther || len(cookies) != 1 || cookies[0].MaxAge >= 0 {
				t.Fatalf("consumed reservation kept browser session: HTTP %d", w.Code)
			}
			if w := response(p, request("POST", reserve, nil)); w.Code != http.StatusSeeOther {
				t.Fatalf("operator could not reserve again immediately: HTTP %d", w.Code)
			}
			if w := response(p, request("POST", release, cookie)); w.Code != http.StatusForbidden {
				t.Fatalf("old cookie could act on the new reservation: HTTP %d", w.Code)
			}
		})
	}
}
