package main

import (
	"context"
	"crypto/rand"
	_ "embed"
	"encoding/hex"
	"html/template"
	"mime"
	"net"
	"net/http"
	"net/netip"
	"net/url"
	"regexp"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"
)

const (
	cookieName  = "device_core_wifi"
	lease       = 5 * time.Minute
	callTimeout = 5 * time.Second
)

type reservation struct {
	browser, token string
	expires        time.Time
	submitted      bool
}

func (s reservation) matches(r *http.Request) bool {
	cookies := r.CookiesNamed(cookieName)
	return s.token != "" && time.Now().Before(s.expires) &&
		len(cookies) == 1 && cookies[0].Value == s.browser
}

type portal struct {
	hotspotAddress netip.Addr
	network        networkClient
	mu             sync.Mutex
	session        reservation
}

func (p *portal) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Cache-Control", "no-store")
	w.Header().Set("Content-Security-Policy", "default-src 'none'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'")
	// no-referrer makes browsers send Origin: null on native form POSTs.
	w.Header().Set("Referrer-Policy", "same-origin")
	w.Header().Set("X-Content-Type-Options", "nosniff")

	local, ok := r.Context().Value(http.LocalAddrContextKey).(*net.TCPAddr)
	host := p.hotspotAddress.String()
	if !ok || local == nil || local.String() != netip.AddrPortFrom(p.hotspotAddress, hotspotPort).String() {
		http.Error(w, "Ce portail est accessible depuis le Wi-Fi de l’appareil.", http.StatusForbidden)
		return
	}
	canonical := r.Host == host && r.URL.Path == "/" && r.URL.RawPath == "" && r.URL.RawQuery == "" && !r.URL.ForceQuery
	if r.Method == http.MethodGet && !canonical {
		http.Redirect(w, r, "http://"+host+"/", http.StatusFound)
		return
	}
	if r.Host != host {
		http.Error(w, "Ce portail est accessible depuis le Wi-Fi de l’appareil.", http.StatusForbidden)
		return
	}
	if r.URL.Path != "/" || r.URL.RawPath != "" || r.URL.RawQuery != "" || r.URL.ForceQuery {
		http.NotFound(w, r)
		return
	}

	switch r.Method {
	case http.MethodGet:
		p.show(w, r)
	case http.MethodPost:
		origins := r.Header.Values("Origin")
		if len(origins) != 1 || origins[0] != "http://"+host {
			http.Error(w, "Origine de la demande refusée.", http.StatusForbidden)
			return
		}
		kind, _, err := mime.ParseMediaType(r.Header.Get("Content-Type"))
		if err != nil || kind != "application/x-www-form-urlencoded" {
			http.Error(w, "Formulaire non pris en charge.", http.StatusUnsupportedMediaType)
			return
		}
		r.Body = http.MaxBytesReader(w, r.Body, 2048)
		if err := r.ParseForm(); err != nil {
			http.Error(w, "Formulaire invalide ou trop volumineux.", http.StatusBadRequest)
			return
		}
		p.submit(w, r)
	default:
		w.Header().Set("Allow", "GET, POST")
		http.Error(w, "Méthode non autorisée.", http.StatusMethodNotAllowed)
	}
}

func (p *portal) submit(w http.ResponseWriter, r *http.Request) {
	action := r.PostForm.Get("action")
	var c candidate
	switch action {
	case "connect":
		var ok bool
		c, ok = parseCandidate(r.PostForm)
		if !ok {
			render(w, http.StatusBadRequest, page{Message: "Vérifiez le réseau et le mot de passe saisis."})
			return
		}
	case "reserve", "scan", "release":
		if len(r.PostForm) != 1 || len(r.PostForm["action"]) != 1 {
			http.Error(w, "Formulaire invalide.", http.StatusBadRequest)
			return
		}
	default:
		http.Error(w, "Action inconnue.", http.StatusBadRequest)
		return
	}

	if !p.mu.TryLock() {
		render(w, http.StatusConflict, page{Message: "Une autre demande est en cours. Revenez au portail dans quelques instants."})
		return
	}
	defer p.mu.Unlock()
	s := &p.session
	if action != "reserve" && (!s.matches(r) || (s.submitted && action != "release")) {
		render(w, http.StatusForbidden, page{Message: "Cette session de configuration n’est plus utilisable. Revenez au portail."})
		return
	}

	ctx, cancel := context.WithTimeout(r.Context(), callTimeout)
	defer cancel()
	state, err := p.network.status(ctx)
	if err != nil || !state.hotspot(p.hotspotAddress) {
		render(w, http.StatusServiceUnavailable, page{Message: "Le point d’accès de l’appareil n’est pas disponible."})
		return
	}
	if state.Phase == "connecting" {
		render(w, http.StatusConflict, page{Message: connectionMessage})
		return
	}

	switch action {
	case "reserve":
		if time.Now().Before(s.expires) {
			render(w, http.StatusConflict, page{Message: "Une session de configuration est déjà ouverte. Revenez au portail ou réessayez dans cinq minutes."})
			return
		}
		expires := time.Now().Add(lease)
		token, err := p.network.reserve(ctx)
		if err != nil || token == "" || len(token) > 64 {
			render(w, http.StatusConflict, page{Message: "La configuration n’est pas disponible pour le moment. Réessayez dans quelques minutes."})
			return
		}
		*s = reservation{browser: rand.Text(), token: token, expires: expires}
		setCookie(w, s.browser, int(time.Until(expires).Seconds()))
	case "release":
		if !s.matches(r) {
			render(w, http.StatusForbidden, page{Message: "La session de configuration a expiré."})
			return
		}
		if err := p.network.release(ctx, s.token); err != nil {
			render(w, http.StatusConflict, page{Message: "La session n’a pas pu être fermée. Elle expire au bout de cinq minutes."})
			return
		}
		*s = reservation{}
		setCookie(w, "", -1)
	case "scan", "connect":
		ok, err := p.network.authorized(ctx, s.token)
		if err != nil || !ok || !s.matches(r) {
			render(w, http.StatusForbidden, page{Message: "La configuration n’est pas autorisée ou la session a expiré. Revenez au portail."})
			return
		}
		if action == "scan" {
			if err := p.network.scan(ctx); err != nil {
				render(w, http.StatusConflict, page{Message: "La recherche des réseaux n’a pas pu démarrer. Réessayez dans quelques instants."})
				return
			}
			render(w, http.StatusOK, page{Message: "Recherche lancée. Revenez au portail dans quelques secondes pour afficher les résultats."})
			return
		}
		// A lost reply does not prove Connect failed; never resubmit this reservation.
		s.submitted = true
		connectCtx, stop := context.WithTimeout(context.WithoutCancel(r.Context()), callTimeout)
		defer stop()
		id, err := p.network.connect(connectCtx, c, s.token)
		if err != nil || id == 0 {
			render(w, http.StatusServiceUnavailable, page{Message: "L’appareil n’a pas confirmé la demande. Si le Wi-Fi de l’appareil disparaît, patientez avant de réessayer."})
			return
		}
		render(w, http.StatusAccepted, page{Message: connectionMessage})
		return
	}
	http.Redirect(w, r, "/", http.StatusSeeOther)
}

func (s status) hotspot(addr netip.Addr) bool {
	return s.Mode == "hotspot" && s.Address == addr.String()
}

func setCookie(w http.ResponseWriter, value string, maxAge int) {
	http.SetCookie(w, &http.Cookie{
		Name: cookieName, Value: value, Path: "/", MaxAge: maxAge,
		HttpOnly: true, SameSite: http.SameSiteStrictMode,
	})
}

var uuidPattern = regexp.MustCompile(`^[a-fA-F0-9]{8}(-[a-fA-F0-9]{4}){3}-[a-fA-F0-9]{12}$`)

func parseCandidate(form url.Values) (candidate, bool) {
	if len(form["action"]) != 1 ||
		len(form["network"]) != 1 || len(form["password"]) != 1 {
		return candidate{}, false
	}
	password := form.Get("password")
	if len(password) > 64 || !utf8.ValidString(password) || strings.ContainsRune(password, 0) {
		return candidate{}, false
	}
	if form.Get("network") == "manual" {
		if len(form) != 5 || len(form["ssid"]) != 1 || len(form["security"]) != 1 {
			return candidate{}, false
		}
		ssid, security := form.Get("ssid"), form.Get("security")
		return candidate{ssid: []byte(ssid), security: security, password: password},
			len(ssid) > 0 && len(ssid) <= 32 && utf8.ValidString(ssid) && validPassword(security, password)
	}
	kind, value, ok := strings.Cut(form.Get("network"), ":")
	if len(form) != 3 || !ok {
		return candidate{}, false
	}
	if kind == "saved" {
		return candidate{uuid: value}, uuidPattern.MatchString(value) && password == ""
	}
	if len(value) < 2 || len(value) > 64 {
		return candidate{}, false
	}
	ssid, err := hex.DecodeString(value)
	return candidate{ssid: ssid, security: kind, password: password}, err == nil && validPassword(kind, password)
}

func validPassword(security, password string) bool {
	n := len(password)
	switch security {
	case "open":
		return n == 0
	case "wpa-psk":
		if n == 64 {
			_, err := hex.DecodeString(password)
			return err == nil
		}
		return n >= 8 && n <= 63
	case "sae":
		return n > 0 && n <= 63
	}
	return false
}

const connectionMessage = "L’appareil essaie de se connecter. Votre téléphone peut perdre le Wi-Fi de l’appareil : vous pouvez fermer cette page. Si l’appareil ne se connecte pas et que son Wi-Fi réapparaît, reconnectez-vous à ce Wi-Fi puis revenez ici."

type option struct{ Value, Label string }

type page struct {
	Message          string
	Start, End, Scan bool
	Options          []option
}

func (p *portal) show(w http.ResponseWriter, r *http.Request) {
	ctx, cancel := context.WithTimeout(r.Context(), callTimeout)
	defer cancel()
	state, err := p.network.status(ctx)
	if err != nil {
		render(w, http.StatusServiceUnavailable, page{Message: "Le service Wi-Fi de l’appareil ne répond pas."})
		return
	}
	if state.Phase == "connecting" {
		render(w, http.StatusOK, page{Message: connectionMessage})
		return
	}
	if !state.hotspot(p.hotspotAddress) {
		render(w, http.StatusServiceUnavailable, page{Message: "Le point d’accès de l’appareil n’est pas disponible."})
		return
	}

	p.mu.Lock()
	s := p.session
	p.mu.Unlock()
	view := page{Start: !time.Now().Before(s.expires), End: s.matches(r)}
	if state.Phase == "failed" || state.Phase == "cancelled" {
		view.Message = "La dernière connexion n’a pas abouti. Vérifiez le réseau et son mot de passe, puis réessayez."
	}
	if view.Start || !view.End || s.submitted {
		if !view.Start && !view.End {
			view.Message = "Une autre session de configuration est ouverte. Réessayez dans quelques minutes."
		} else if s.submitted && view.Message == "" {
			view.Message = "Pour envoyer une nouvelle demande, terminez cette session puis recommencez la configuration."
		}
		render(w, http.StatusOK, view)
		return
	}
	ok, err := p.network.authorized(ctx, s.token)
	if err != nil || !ok || !s.matches(r) {
		view.Message = "L’autorisation de l’appareil n’est pas encore disponible. Actualisez la page dans quelques instants."
		render(w, http.StatusOK, view)
		return
	}

	profiles, errProfiles := p.network.profiles(ctx)
	networks, errNetworks := p.network.networks(ctx)
	for _, saved := range profiles {
		if uuidPattern.MatchString(saved.UUID) && len(saved.SSID) > 0 && len(saved.SSID) <= 32 {
			view.Options = append(view.Options, option{"saved:" + saved.UUID, strconv.Quote(string(saved.SSID)) + " (enregistré)"})
		}
	}
	for _, network := range networks {
		if len(network.SSID) == 0 || len(network.SSID) > 32 {
			continue
		}
		label := strconv.Quote(string(network.SSID))
		switch network.Security {
		case "open":
			label += " (ouvert)"
		case "wpa-psk":
			label += " (WPA)"
		case "sae":
			label += " (WPA3)"
		default:
			continue
		}
		view.Options = append(view.Options, option{network.Security + ":" + hex.EncodeToString(network.SSID), label})
	}
	view.Scan = true
	if errProfiles != nil || errNetworks != nil {
		view.Message = "La liste des réseaux est incomplète. Actualisez la page dans quelques instants."
	}
	render(w, http.StatusOK, view)
}

//go:embed portal.html
var pageHTML string

var portalTemplate = template.Must(template.New("portal").Parse(pageHTML))

func render(w http.ResponseWriter, code int, view page) {
	w.Header().Set("Content-Type", "text/html; charset=utf-8")
	w.WriteHeader(code)
	_ = portalTemplate.Execute(w, view)
}
