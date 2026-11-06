package main

import (
	"context"
	"errors"
	"net"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"net/url"
	"strconv"
	"strings"
	"testing"
	"time"
)

const (
	controllerToken = "private-controller-reservation"
	testHost        = "10.77.0.1"
	testOrigin      = "http://" + testHost
)

var testHotspot = netip.MustParseAddr(testHost)

type testNetwork struct {
	statusReads                           int
	state                                 status
	statusError                           error
	allowed                               bool
	reserved, scans, released, connects   int
	last                                  candidate
	onAuthorized                          func()
	authorizedError                       error
	onReserve                             func()
	reserveError, releaseError, scanError error
	releasedToken                         string
	savedProfiles                         []profile
	onConnect                             func(context.Context) error
	accessPoints                          []accessPoint
	networksError                         error
}

func (n *testNetwork) status(context.Context) (status, error) {
	n.statusReads++
	return n.state, n.statusError
}
func (n *testNetwork) networks(context.Context) ([]accessPoint, error) {
	if n.accessPoints != nil || n.networksError != nil {
		return n.accessPoints, n.networksError
	}
	return []accessPoint{{SSID: []byte{0xff, '<', '>'}, Security: "wpa-psk"}}, nil
}
func (n *testNetwork) profiles(context.Context) ([]profile, error) { return n.savedProfiles, nil }
func (n *testNetwork) scan(context.Context) error                  { n.scans++; return n.scanError }
func (n *testNetwork) reserve(context.Context) (string, error) {
	n.reserved++
	if n.onReserve != nil {
		n.onReserve()
	}
	return controllerToken, n.reserveError
}
func (n *testNetwork) authorized(_ context.Context, token string) (bool, error) {
	if n.onAuthorized != nil {
		n.onAuthorized()
	}
	return token == controllerToken && n.allowed, n.authorizedError
}
func (n *testNetwork) connect(ctx context.Context, c candidate, token string) (uint64, error) {
	n.connects++
	n.last = c
	if token != controllerToken {
		return 0, errors.New("wrong token")
	}
	if n.onConnect != nil {
		return 1, n.onConnect(ctx)
	}
	return 1, nil
}
func (n *testNetwork) release(_ context.Context, token string) error {
	if token != controllerToken {
		return errors.New("wrong token")
	}
	n.released++
	n.releasedToken = token
	return n.releaseError
}

func request(method string, form url.Values, cookie *http.Cookie) *http.Request {
	r := httptest.NewRequest(method, testOrigin+"/", strings.NewReader(form.Encode()))
	r = r.WithContext(context.WithValue(r.Context(), http.LocalAddrContextKey,
		&net.TCPAddr{IP: net.ParseIP(testHost), Port: 80}))
	if method == http.MethodPost {
		r.Header.Set("Origin", testOrigin)
		r.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	}
	if cookie != nil {
		r.AddCookie(cookie)
	}
	return r
}

func response(p *portal, r *http.Request) *httptest.ResponseRecorder {
	w := httptest.NewRecorder()
	p.ServeHTTP(w, r)
	return w
}

func setup() (*portal, *testNetwork) {
	n := &testNetwork{state: status{Mode: "hotspot", Address: testHost, Phase: "idle"}, allowed: true}
	return &portal{hotspotAddress: testHotspot, network: n}, n
}

func connectForm() url.Values {
	return url.Values{"action": {"connect"}, "network": {"wpa-psk:ff3c3e"}, "password": {"test-passphrase"}}
}

func TestBrowserMetadataDoesNotBlockSetup(t *testing.T) {
	for _, tc := range []struct {
		name   string
		change func(*http.Request)
	}{
		{"missing Origin and cookie", func(r *http.Request) { r.Header.Del("Origin") }},
		{"null Origin", func(r *http.Request) { r.Header.Set("Origin", "null") }},
		{"foreign Origin", func(r *http.Request) { r.Header.Set("Origin", "http://foreign.example") }},
		{"duplicate Origin", func(r *http.Request) { r.Header.Add("Origin", "null") }},
		{"Host with port", func(r *http.Request) { r.Host += ":80" }},
		{"Host alias", func(r *http.Request) { r.Host = "probe.example" }},
		{"old Host", func(r *http.Request) { r.Host = "10.41.0.1" }},
		{"unrelated cookies", func(r *http.Request) { r.Header.Set("Cookie", "device_core_wifi=old; device_core_wifi=other") }},
	} {
		t.Run(tc.name, func(t *testing.T) {
			p, n := setup()
			r := request("POST", connectForm(), nil)
			tc.change(r)
			w := response(p, r)
			if w.Code != http.StatusAccepted || n.connects != 1 || n.reserved != 1 || n.released != 0 ||
				len(w.Result().Cookies()) != 0 || strings.Contains(w.Body.String(), controllerToken) ||
				w.Header().Get("Content-Security-Policy") != "" {
				t.Fatalf("direct connect: HTTP %d, connect=%d reserve=%d release=%d", w.Code, n.connects, n.reserved, n.released)
			}
		})
	}
}

func TestCaptiveDiscovery(t *testing.T) {
	for _, target := range []string{
		"http://probe.example/",
		"http://probe.example/generate_204?next=https://foreign.example/",
		testOrigin + ":80/",
		testOrigin + "/hotspot-detect.html",
		testOrigin + "/?next=https://foreign.example/",
		testOrigin + "/?",
		testOrigin + "/%2f",
	} {
		t.Run(target, func(t *testing.T) {
			// A discovery request must not need any controller call.
			p := &portal{hotspotAddress: testHotspot}
			r := httptest.NewRequest(http.MethodGet, target, nil).WithContext(request("GET", nil, nil).Context())
			r.Header.Set("X-Forwarded-Host", "foreign.example")
			w := response(p, r)
			if w.Code != http.StatusFound || w.Header().Get("Location") != testOrigin+"/" ||
				len(w.Result().Cookies()) != 0 || p.token != "" || w.Header().Get("Cache-Control") != "no-store" {
				t.Fatalf("discovery: HTTP %d, location=%q, session=%v", w.Code, w.Header().Get("Location"), p.token)
			}
		})
	}

	n := &testNetwork{state: status{Mode: "hotspot", Address: testHost, Phase: "idle"}}
	p := &portal{hotspotAddress: testHotspot, network: n}
	w := response(p, request("GET", nil, nil))
	if w.Code != http.StatusOK || w.Header().Get("Location") != "" || len(w.Result().Cookies()) != 0 ||
		n.statusReads != 1 || n.reserved != 0 || !strings.Contains(w.Body.String(), `name="ssid"`) ||
		w.Header().Get("Referrer-Policy") != "same-origin" {
		t.Fatalf("canonical page: HTTP %d, status reads=%d reservations=%d, referrer policy=%q", w.Code, n.statusReads, n.reserved, w.Header().Get("Referrer-Policy"))
	}
}

func TestCaptiveDiscoveryRequiresHotspotSocket(t *testing.T) {
	for _, local := range []*net.TCPAddr{
		nil,
		{IP: net.ParseIP("192.168.1.10"), Port: hotspotPort},
		{IP: net.ParseIP(testHost), Port: 8080},
	} {
		p := &portal{hotspotAddress: testHotspot}
		r := httptest.NewRequest(http.MethodGet, "http://probe.example/generate_204", nil)
		if local != nil {
			r = r.WithContext(context.WithValue(r.Context(), http.LocalAddrContextKey, local))
		}
		r.Header.Set("X-Forwarded-Host", testHost)
		w := response(p, r)
		if w.Code != http.StatusForbidden || w.Header().Get("Location") != "" || len(w.Result().Cookies()) != 0 {
			t.Fatalf("socket %v: HTTP %d, location=%q", local, w.Code, w.Header().Get("Location"))
		}
	}
}

func TestConfiguredHotspotAddress(t *testing.T) {
	p, n := setup()
	r := request("POST", connectForm(), nil)
	r = r.WithContext(context.WithValue(r.Context(), http.LocalAddrContextKey,
		&net.TCPAddr{IP: net.ParseIP("10.41.0.1"), Port: hotspotPort}))
	if w := response(p, r); w.Code != http.StatusForbidden || n.statusReads != 0 || n.reserved != 0 {
		t.Fatalf("old socket: HTTP %d; status reads=%d reservations=%d", w.Code, n.statusReads, n.reserved)
	}
	n.state.Address = "10.41.0.1"
	if w := response(p, request("POST", connectForm(), nil)); w.Code != http.StatusServiceUnavailable || n.reserved != 0 {
		t.Fatalf("mismatched controller address: HTTP %d; reservations=%d", w.Code, n.reserved)
	}
	n.state.Address = testHost
	if w := response(p, request("POST", connectForm(), nil)); w.Code != http.StatusAccepted || n.reserved != 1 {
		t.Fatalf("matching addresses: HTTP %d; reservations=%d", w.Code, n.reserved)
	}
}

func TestRejectsUnauthorizedWriter(t *testing.T) {
	for _, tc := range []struct {
		name   string
		change func(*http.Request, *portal, *testNetwork)
	}{
		{"LAN socket", func(r *http.Request, _ *portal, _ *testNetwork) {
			*r = *r.WithContext(context.WithValue(r.Context(), http.LocalAddrContextKey,
				&net.TCPAddr{IP: net.ParseIP("192.168.1.10"), Port: 80}))
			r.Header.Set("X-Forwarded-Host", testHost)
		}},
		{"wrong port", func(r *http.Request, _ *portal, _ *testNetwork) {
			*r = *r.WithContext(context.WithValue(r.Context(), http.LocalAddrContextKey,
				&net.TCPAddr{IP: net.ParseIP(testHost), Port: 8080}))
		}},
		{"discovery path", func(r *http.Request, _ *portal, _ *testNetwork) { r.URL.Path = "/generate_204" }},
		{"query", func(r *http.Request, _ *portal, _ *testNetwork) { r.URL.RawQuery = "next=foreign" }},
		{"empty query", func(r *http.Request, _ *portal, _ *testNetwork) { r.URL.ForceQuery = true }},
		{"controller revoked", func(_ *http.Request, _ *portal, n *testNetwork) { n.allowed = false }},
		{"controller unavailable", func(_ *http.Request, _ *portal, n *testNetwork) { n.statusError = errors.New("offline") }},
		{"unknown phase", func(_ *http.Request, _ *portal, n *testNetwork) { n.state.Phase = "unknown" }},

		{"hotspot gone", func(_ *http.Request, _ *portal, n *testNetwork) { n.state.Mode = "client" }},
		{"hotspot moved", func(_ *http.Request, _ *portal, n *testNetwork) { n.state.Address = "10.42.0.1" }},
	} {
		t.Run(tc.name, func(t *testing.T) {
			p, n := setup()
			r := request("POST", connectForm(), nil)
			tc.change(r, p, n)
			w := response(p, r)
			if w.Code < 400 || n.connects != 0 || n.scans != 0 || n.released != 0 {
				t.Fatalf("HTTP %d; connect=%d scan=%d release=%d", w.Code, n.connects, n.scans, n.released)
			}
		})
	}
}

func TestConnectSurvivesDisconnect(t *testing.T) {
	for _, ambiguous := range []bool{false, true} {
		p, n := setup()
		r := request("POST", connectForm(), nil)
		ctx, cancel := context.WithCancel(r.Context())
		defer cancel()
		n.onAuthorized = cancel
		n.onConnect = func(ctx context.Context) error {
			if ctx.Err() != nil {
				t.Fatal("controller submission cancelled when phone disconnected")
			}
			if deadline, ok := ctx.Deadline(); !ok || time.Until(deadline) > callTimeout {
				t.Fatal("controller submission has no bounded deadline")
			}
			if ambiguous {
				return errors.New("lost reply with test-passphrase")
			}
			return nil
		}
		w := response(p, r.WithContext(ctx))
		want := http.StatusAccepted
		if ambiguous {
			want = http.StatusServiceUnavailable
		}
		if w.Code != want || n.connects != 1 || string(n.last.ssid) != "\xff<>" ||
			strings.Contains(w.Body.String(), "test-passphrase") {
			t.Fatalf("unexpected result: HTTP %d, %d calls", w.Code, n.connects)
		}
		if n.released != 0 || p.token != controllerToken {
			t.Fatal("dispatch released the token")
		}
	}
}

func TestControllerReservationLifecycle(t *testing.T) {
	for _, phase := range []string{"idle", "failed", "cancelled"} {
		t.Run("replace invalid token/"+phase, func(t *testing.T) {
			p, n := setup()
			p.token = controllerToken
			n.allowed = false
			n.state.Phase = phase
			n.onReserve = func() { n.allowed = true }
			w := response(p, request("POST", connectForm(), nil))
			if w.Code != http.StatusAccepted || n.connects != 1 || n.reserved != 1 || n.released != 1 || n.releasedToken != controllerToken {
				t.Fatalf("replacement: HTTP %d, connect=%d reserve=%d release=%d token=%q", w.Code, n.connects, n.reserved, n.released, n.releasedToken)
			}
		})
	}
	for _, failure := range []string{"authorization", "release", "reserve", "scan"} {
		t.Run(failure+" error", func(t *testing.T) {
			p, n := setup()
			p.token = controllerToken
			n.allowed = false
			err := errors.New("private failure with test-passphrase")
			form := connectForm()
			switch failure {
			case "authorization":
				n.authorizedError = err
			case "release":
				n.releaseError = err
			case "reserve":
				n.reserveError = err
			case "scan":
				n.scanError = err
				form = url.Values{"action": {"scan"}}
			}
			w := response(p, request("POST", form, nil))
			if w.Code < 400 || n.connects != 0 || strings.Contains(w.Body.String(), "test-passphrase") {
				t.Fatalf("failed operation reached connect or leaked its error: HTTP %d", w.Code)
			}
			if (failure == "authorization" || failure == "release") && (p.token != controllerToken || n.reserved != 0) {
				t.Fatal("uncertain authorization/release discarded the token or reserved again")
			}
		})
	}
}

func TestPendingConnect(t *testing.T) {
	for _, ambiguous := range []bool{false, true} {
		t.Run("lost reply="+strconv.FormatBool(ambiguous), func(t *testing.T) {
			p, n := setup()
			n.state.AttemptID, n.state.Phase = 12, "failed"
			n.onReserve = func() { n.allowed = true }
			n.onConnect = func(context.Context) error {
				n.state.AttemptID++
				n.state.Phase, n.allowed = "connecting", false
				if ambiguous {
					return errors.New("reply lost")
				}
				return nil
			}
			w := response(p, request("POST", connectForm(), nil))
			if (!ambiguous && w.Code != http.StatusAccepted) || (ambiguous && w.Code != http.StatusServiceUnavailable) || n.released != 0 {
				t.Fatalf("initial dispatch: HTTP %d, releases=%d", w.Code, n.released)
			}
			for _, phase := range []string{"connecting", "unknown"} {
				n.state.Phase = phase
				if w := response(p, request("POST", connectForm(), nil)); w.Code != http.StatusConflict || n.connects != 1 || n.released != 0 || n.reserved != 1 {
					t.Fatalf("pending %s: HTTP %d, connect=%d release=%d reserve=%d", phase, w.Code, n.connects, n.released, n.reserved)
				}
			}
			n.statusError = errors.New("offline")
			if w := response(p, request("POST", connectForm(), nil)); w.Code != http.StatusServiceUnavailable || n.released != 0 {
				t.Fatalf("unknown controller state: HTTP %d, releases=%d", w.Code, n.released)
			}
			n.statusError = nil
			n.state.AttemptID, n.state.Phase = 13, "cancelled"
			w = response(p, request("POST", connectForm(), nil))
			if n.connects != 2 || n.reserved != 2 || n.released != 1 || n.releasedToken != controllerToken {
				t.Fatalf("manual retry after confirmed cancellation: HTTP %d, connect=%d reserve=%d release=%d", w.Code, n.connects, n.reserved, n.released)
			}
		})
	}
}

func TestStatusRecheckedBeforeRelease(t *testing.T) {
	for _, phase := range []string{"connecting", "unknown", "unavailable", "moved"} {
		t.Run(phase, func(t *testing.T) {
			p, n := setup()
			p.token = controllerToken
			n.allowed = false
			n.onAuthorized = func() {
				n.state.Phase = phase
				if phase == "unavailable" {
					n.statusError = errors.New("offline")
				}
				if phase == "moved" {
					n.state.Phase, n.state.Address = "idle", "10.42.0.1"
				}
			}
			w := response(p, request("POST", connectForm(), nil))
			if w.Code != http.StatusConflict || n.released != 0 || n.reserved != 0 || n.connects != 0 || p.token != controllerToken {
				t.Fatalf("state changed during authorization: HTTP %d release=%d reserve=%d connect=%d", w.Code, n.released, n.reserved, n.connects)
			}
		})
	}
}

func TestConnectTimeoutAllowsLaterManualRequest(t *testing.T) {
	p, n := setup()
	n.onConnect = func(ctx context.Context) error { <-ctx.Done(); return ctx.Err() }
	w := response(p, request("POST", connectForm(), nil))
	if w.Code != http.StatusServiceUnavailable || n.released != 0 || n.connects != 1 || p.token != controllerToken {
		t.Fatalf("timeout: HTTP %d release=%d connect=%d", w.Code, n.released, n.connects)
	}
	n.onConnect = nil
	if w := response(p, request("POST", connectForm(), nil)); w.Code != http.StatusAccepted || n.connects != 2 {
		t.Fatalf("later manual request blocked: HTTP %d connect=%d", w.Code, n.connects)
	}
}

func TestManualRetryAfterControllerRejectionOrRestart(t *testing.T) {
	for _, reason := range []string{"authorization expired", "profile changed", "controller restart"} {
		t.Run(reason, func(t *testing.T) {
			p, n := setup()
			n.state.AttemptID, n.state.Phase = 12, "failed"
			n.onConnect = func(context.Context) error { return errors.New(reason) }
			if w := response(p, request("POST", connectForm(), nil)); w.Code != http.StatusServiceUnavailable || n.connects != 1 || n.released != 0 {
				t.Fatalf("rejected dispatch: HTTP %d connect=%d release=%d", w.Code, n.connects, n.released)
			}
			n.onConnect = nil
			wantReserves, wantReleases := 1, 0
			if reason != "profile changed" {
				n.allowed = false
				n.onReserve = func() { n.allowed = true }
				wantReserves, wantReleases = 2, 1
			}
			if reason == "controller restart" {
				n.state.AttemptID, n.state.Phase = 0, "idle"
			}
			if w := response(p, request("POST", connectForm(), nil)); w.Code != http.StatusAccepted || n.connects != 2 || n.reserved != wantReserves || n.released != wantReleases {
				t.Fatalf("manual retry blocked: HTTP %d connect=%d reserve=%d release=%d", w.Code, n.connects, n.reserved, n.released)
			}
		})
	}
}

func TestDirectSetup(t *testing.T) {
	p, n := setup()
	const uuid = "64657669-6365-4000-8000-000000000002"
	n.savedProfiles = []profile{{UUID: uuid, SSID: []byte("Saved Wi-Fi")}}
	w := response(p, request("GET", nil, nil))
	body := w.Body.String()
	if w.Code != http.StatusOK || n.reserved != 0 || n.released != 0 ||
		!strings.Contains(body, "saved:"+uuid) || !strings.Contains(body, `value="wpa-psk:ff3c3e"`) ||
		!strings.Contains(body, `name="ssid"`) || !strings.Contains(body, `value="scan"`) ||
		strings.Contains(body, `value="reserve"`) || strings.Contains(body, `value="release"`) || len(w.Result().Cookies()) != 0 {
		t.Fatalf("initial GET did not expose direct setup: HTTP %d, reserves=%d", w.Code, n.reserved)
	}
	if w := response(p, request("POST", url.Values{"action": {"scan"}}, nil)); w.Code != http.StatusOK || n.scans != 1 || n.reserved != 0 {
		t.Fatalf("scan without reservation: HTTP %d, scans=%d reserves=%d", w.Code, n.scans, n.reserved)
	}
	for _, obsolete := range []string{"reserve", "release"} {
		if w := response(p, request("POST", url.Values{"action": {obsolete}}, nil)); w.Code != http.StatusBadRequest || n.reserved != 0 || n.released != 0 {
			t.Fatalf("obsolete browser action %s: HTTP %d", obsolete, w.Code)
		}
	}
	form := url.Values{"action": {"connect"}, "network": {"saved:" + uuid}, "password": {""}}
	if w := response(p, request("POST", form, nil)); w.Code != http.StatusAccepted || n.last.uuid != uuid {
		t.Fatalf("saved profile: HTTP %d, uuid=%q", w.Code, n.last.uuid)
	}
}

func TestFormBoundsAndSSIDEncoding(t *testing.T) {
	p, n := setup()
	w := response(p, request("GET", nil, nil))
	body := w.Body.String()
	if w.Code != http.StatusOK || !strings.Contains(body, `value="wpa-psk:ff3c3e"`) ||
		!strings.Contains(body, `\xff&lt;&gt;`) || strings.Contains(body, controllerToken) {
		t.Fatalf("SSID lost bytes or leaked into markup: %s", body)
	}
	for _, form := range []url.Values{
		{"action": {"connect"}, "network": {"open:ff"}, "password": {strings.Repeat("x", 2048)}},
		{"action": {"connect"}, "network": {"open:ff", "open:ee"}, "password": {""}},
		{"action": {"connect"}, "network": {"open:" + strings.Repeat("aa", 33)}, "password": {""}},
		{"action": {"connect"}, "network": {"sae:ff"}, "password": {"\xff"}},
		{"action": {"connect"}, "network": {"wpa-psk:ff"}, "password": {"short"}},
		{"action": {"connect"}, "network": {"saved:not-a-uuid"}, "password": {""}},
	} {
		if w := response(p, request("POST", form, nil)); w.Code != http.StatusBadRequest || n.connects != 0 {
			t.Fatalf("invalid form reached writer: HTTP %d", w.Code)
		}
	}
}

func TestManualNetworkWithoutScan(t *testing.T) {
	for _, failedScan := range []bool{false, true} {
		p, n := setup()
		n.accessPoints = []accessPoint{}
		if failedScan {
			n.networksError = errors.New("scan unavailable")
		}
		w := response(p, request("GET", nil, nil))
		if w.Code != http.StatusOK || !strings.Contains(w.Body.String(), `name="ssid"`) ||
			!strings.Contains(w.Body.String(), `name="security"`) {
			t.Fatal("manual entry missing without scan results")
		}
		form := url.Values{"action": {"connect"}, "network": {"manual"},
			"ssid": {" Ma Box é "}, "security": {"wpa-psk"}, "password": {"test-passphrase"}}
		if w := response(p, request("POST", form, nil)); w.Code != http.StatusAccepted || n.connects != 1 ||
			string(n.last.ssid) != " Ma Box é " || n.last.security != "wpa-psk" || n.last.password != "test-passphrase" {
			t.Fatal("manual network did not reach the existing connect path unchanged")
		}

	}
}

func TestOwnHotspotIsNotAConnectionChoice(t *testing.T) {
	p, n := setup()
	n.state.SSID = []byte("Device recovery")
	n.accessPoints = []accessPoint{
		{SSID: n.state.SSID, Security: "open"},
		{SSID: []byte("Home"), Security: "wpa-psk"},
	}
	w := response(p, request("GET", nil, nil))
	if w.Code != http.StatusOK || strings.Contains(w.Body.String(), "Device recovery") ||
		!strings.Contains(w.Body.String(), `value="wpa-psk:486f6d65"`) ||
		!strings.Contains(w.Body.String(), `name="ssid"`) {
		t.Fatal("own hotspot should be excluded while station choices and manual entry remain")
	}
}

func TestManualNetworkValidation(t *testing.T) {
	valid := url.Values{"action": {"connect"}, "network": {"manual"},
		"ssid": {strings.Repeat("é", 16)}, "security": {"wpa-psk"}, "password": {"test-passphrase"}}
	for _, security := range []string{"wpa-psk", "sae", "open"} {
		form := url.Values{}
		for key, values := range valid {
			form.Set(key, values[0])
		}
		form.Set("security", security)
		if security == "open" {
			form.Set("password", "")
		}
		if c, ok := parseCandidate(form); !ok || string(c.ssid) != valid.Get("ssid") || c.security != security {
			t.Fatalf("supported manual network rejected: %s", security)
		}
	}
	for _, mutate := range []func(url.Values){
		func(f url.Values) { f.Set("ssid", "") },
		func(f url.Values) { f.Set("ssid", strings.Repeat("é", 17)) },
		func(f url.Values) { f.Set("ssid", "\xff") },
		func(f url.Values) { f.Add("ssid", "other") },
		func(f url.Values) { f.Add("security", "open") },
		func(f url.Values) { f.Set("security", "unknown") },
		func(f url.Values) { f.Set("password", "short") },
		func(f url.Values) { f.Set("extra", "value") },
	} {
		p, n := setup()
		form := url.Values{}
		for key, values := range valid {
			form.Set(key, values[0])
		}
		mutate(form)
		if w := response(p, request("POST", form, nil)); w.Code != http.StatusBadRequest || n.connects != 0 {
			t.Fatal("invalid manual network reached controller")
		}
	}
}

func TestConcurrentConnectDoesNotReachControllerTwice(t *testing.T) {
	p, n := setup()
	entered := make(chan struct{})
	unblock := make(chan struct{})
	first := make(chan int, 1)
	n.onConnect = func(ctx context.Context) error {
		close(entered)
		select {
		case <-unblock:
			return nil
		case <-ctx.Done():
			return ctx.Err()
		}
	}
	go func() { first <- response(p, request("POST", connectForm(), nil)).Code }()
	select {
	case <-entered:
	case <-time.After(callTimeout):
		t.Fatal("first request did not reach the controller")
	}
	second := response(p, request("POST", connectForm(), nil))
	close(unblock)
	if initial := <-first; initial != http.StatusAccepted || second.Code != http.StatusConflict || n.connects != 1 {
		t.Fatalf("first HTTP %d, second HTTP %d, %d controller submissions", initial, second.Code, n.connects)
	}
}
