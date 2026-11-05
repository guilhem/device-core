package main

import (
	"context"
	"errors"
	"net"
	"net/http"
	"net/http/httptest"
	"net/netip"
	"net/url"
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
	statusReads                         int
	state                               status
	allowed                             bool
	reserved, scans, released, connects int
	last                                candidate
	onAuthorized                        func()
	onConnect                           func(context.Context) error
}

func (n *testNetwork) status(context.Context) (status, error) { n.statusReads++; return n.state, nil }
func (*testNetwork) networks(context.Context) ([]accessPoint, error) {
	return []accessPoint{{SSID: []byte{0xff, '<', '>'}, Security: "wpa-psk"}}, nil
}
func (*testNetwork) profiles(context.Context) ([]profile, error) { return nil, nil }
func (n *testNetwork) scan(context.Context) error                { n.scans++; return nil }
func (n *testNetwork) reserve(context.Context) (string, error) {
	n.reserved++
	return controllerToken, nil
}
func (n *testNetwork) authorized(_ context.Context, token string) (bool, error) {
	if n.onAuthorized != nil {
		n.onAuthorized()
	}
	return token == controllerToken && n.allowed, nil
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
	return nil
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

func setup(t *testing.T) (*portal, *testNetwork, *http.Cookie) {
	t.Helper()
	n := &testNetwork{state: status{Mode: "hotspot", Address: testHost}, allowed: true}
	p := &portal{hotspotAddress: testHotspot, network: n}
	w := response(p, request("POST", url.Values{"action": {"reserve"}}, nil))
	cookies := w.Result().Cookies()
	if w.Code != http.StatusSeeOther || n.reserved != 1 || len(cookies) != 1 {
		t.Fatalf("reserve: HTTP %d, %d cookies, %d reservations", w.Code, len(cookies), n.reserved)
	}
	cookie := cookies[0]
	if !cookie.HttpOnly || cookie.SameSite != http.SameSiteStrictMode || cookie.Domain != "" ||
		cookie.Value == "" || cookie.Value == controllerToken || cookie.MaxAge <= 0 || cookie.MaxAge > 300 {
		t.Fatal("unsafe reservation cookie")
	}
	return p, n, cookie
}

func connectForm() url.Values {
	return url.Values{"action": {"connect"}, "network": {"wpa-psk:ff3c3e"}, "password": {"test-passphrase"}}
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
				len(w.Result().Cookies()) != 0 || p.session != (reservation{}) || w.Header().Get("Cache-Control") != "no-store" {
				t.Fatalf("discovery: HTTP %d, location=%q, session=%v", w.Code, w.Header().Get("Location"), p.session)
			}
		})
	}

	n := &testNetwork{state: status{Mode: "hotspot", Address: testHost}}
	p := &portal{hotspotAddress: testHotspot, network: n}
	w := response(p, request("GET", nil, nil))
	if w.Code != http.StatusOK || w.Header().Get("Location") != "" || len(w.Result().Cookies()) != 0 ||
		n.statusReads != 1 || n.reserved != 0 || !strings.Contains(w.Body.String(), "Configurer le Wi-Fi") {
		t.Fatalf("canonical page: HTTP %d, status reads=%d reservations=%d", w.Code, n.statusReads, n.reserved)
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
	const oldHost = "10.41.0.1"
	n := &testNetwork{state: status{Mode: "hotspot", Address: testHost}}
	p := &portal{hotspotAddress: testHotspot, network: n}
	reserve := url.Values{"action": {"reserve"}}
	for _, tc := range []struct {
		name   string
		change func(*http.Request) *http.Request
	}{
		{"old local address", func(r *http.Request) *http.Request {
			return r.WithContext(context.WithValue(r.Context(), http.LocalAddrContextKey,
				&net.TCPAddr{IP: net.ParseIP(oldHost), Port: hotspotPort}))
		}},
		{"old host", func(r *http.Request) *http.Request {
			r.Host = oldHost
			r.Header.Set("X-Forwarded-Host", testHost)
			return r
		}},
		{"old origin", func(r *http.Request) *http.Request {
			r.Header.Set("Origin", "http://"+oldHost)
			return r
		}},
	} {
		t.Run(tc.name, func(t *testing.T) {
			w := response(p, tc.change(request("POST", reserve, nil)))
			if w.Code != http.StatusForbidden || n.statusReads != 0 || n.reserved != 0 {
				t.Fatalf("HTTP %d; status reads=%d reservations=%d", w.Code, n.statusReads, n.reserved)
			}
		})
	}

	n.state.Address = oldHost
	if w := response(p, request("POST", reserve, nil)); w.Code != http.StatusServiceUnavailable ||
		n.statusReads != 1 || n.reserved != 0 {
		t.Fatalf("mismatched controller address: HTTP %d; status reads=%d reservations=%d", w.Code, n.statusReads, n.reserved)
	}
	n.state.Address = testHost
	if w := response(p, request("POST", reserve, nil)); w.Code != http.StatusSeeOther || n.reserved != 1 {
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
		{"host alias", func(r *http.Request, _ *portal, _ *testNetwork) { r.Host += ":80" }},
		{"discovery path", func(r *http.Request, _ *portal, _ *testNetwork) { r.URL.Path = "/generate_204" }},
		{"query", func(r *http.Request, _ *portal, _ *testNetwork) { r.URL.RawQuery = "next=foreign" }},
		{"empty query", func(r *http.Request, _ *portal, _ *testNetwork) { r.URL.ForceQuery = true }},
		{"cross origin", func(r *http.Request, _ *portal, _ *testNetwork) { r.Header.Set("Origin", "http://foreign.example") }},
		{"missing origin", func(r *http.Request, _ *portal, _ *testNetwork) { r.Header.Del("Origin") }},
		{"duplicate origin", func(r *http.Request, _ *portal, _ *testNetwork) { r.Header.Add("Origin", testOrigin) }},
		{"missing cookie", func(r *http.Request, _ *portal, _ *testNetwork) { r.Header.Del("Cookie") }},
		{"controller token as cookie", func(r *http.Request, _ *portal, _ *testNetwork) {
			r.Header.Set("Cookie", cookieName+"="+controllerToken)
		}},
		{"duplicate cookie", func(r *http.Request, _ *portal, _ *testNetwork) {
			r.AddCookie(&http.Cookie{Name: cookieName, Value: "other"})
		}},
		{"expired cookie", func(_ *http.Request, p *portal, _ *testNetwork) { p.session.expires = time.Now().Add(-time.Second) }},
		{"expired during authorization", func(_ *http.Request, p *portal, n *testNetwork) {
			n.onAuthorized = func() { p.session.expires = time.Now().Add(-time.Second) }
		}},
		{"controller revoked", func(_ *http.Request, _ *portal, n *testNetwork) { n.allowed = false }},
		{"hotspot gone", func(_ *http.Request, _ *portal, n *testNetwork) { n.state.Mode = "client" }},
		{"hotspot moved", func(_ *http.Request, _ *portal, n *testNetwork) { n.state.Address = "10.42.0.1" }},
	} {
		t.Run(tc.name, func(t *testing.T) {
			p, n, cookie := setup(t)
			r := request("POST", connectForm(), cookie)
			tc.change(r, p, n)
			w := response(p, r)
			if w.Code < 400 || n.connects != 0 || n.scans != 0 || n.released != 0 {
				t.Fatalf("HTTP %d; connect=%d scan=%d release=%d", w.Code, n.connects, n.scans, n.released)
			}
		})
	}
}

func TestConnectSurvivesDisconnectAndDoesNotReplay(t *testing.T) {
	for _, ambiguous := range []bool{false, true} {
		p, n, cookie := setup(t)
		r := request("POST", connectForm(), cookie)
		ctx, cancel := context.WithCancel(r.Context())
		defer cancel()
		n.onAuthorized = cancel
		n.onConnect = func(ctx context.Context) error {
			if ctx.Err() != nil {
				t.Fatal("controller submission cancelled when phone disconnected")
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
		if w := response(p, request("POST", connectForm(), cookie)); w.Code != http.StatusForbidden || n.connects != 1 {
			t.Fatalf("replay: HTTP %d, %d calls", w.Code, n.connects)
		}
	}
}

func TestScanAndReleaseUseBrowserReservation(t *testing.T) {
	p, n, cookie := setup(t)
	scan := url.Values{"action": {"scan"}}
	release := url.Values{"action": {"release"}}
	if w := response(p, request("POST", scan, nil)); w.Code != http.StatusForbidden || n.scans != 0 {
		t.Fatal("scan without browser reservation")
	}
	if w := response(p, request("POST", scan, cookie)); w.Code != http.StatusOK || n.scans != 1 {
		t.Fatal("authorized scan rejected")
	}
	if w := response(p, request("POST", release, nil)); w.Code != http.StatusForbidden || n.released != 0 {
		t.Fatal("release without browser reservation")
	}
	w := response(p, request("POST", release, cookie))
	cookies := w.Result().Cookies()
	if w.Code != http.StatusSeeOther || n.released != 1 || len(cookies) != 1 || cookies[0].MaxAge >= 0 {
		t.Fatal("release did not clear session")
	}
}

func TestFormBoundsAndSSIDEncoding(t *testing.T) {
	p, n, cookie := setup(t)
	w := response(p, request("GET", nil, cookie))
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
		if w := response(p, request("POST", form, cookie)); w.Code != http.StatusBadRequest || n.connects != 0 {
			t.Fatalf("invalid form reached writer: HTTP %d", w.Code)
		}
	}
}

func TestConcurrentConnectDoesNotReachControllerTwice(t *testing.T) {
	p, n, cookie := setup(t)
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
	go func() { first <- response(p, request("POST", connectForm(), cookie)).Code }()
	select {
	case <-entered:
	case <-time.After(callTimeout):
		t.Fatal("first request did not reach the controller")
	}
	second := response(p, request("POST", connectForm(), cookie))
	close(unblock)
	if initial := <-first; initial != http.StatusAccepted || second.Code != http.StatusConflict || n.connects != 1 {
		t.Fatalf("first HTTP %d, second HTTP %d, %d controller submissions", initial, second.Code, n.connects)
	}
}
