package main

import (
	"errors"
	"flag"
	"log"
	"net"
	"net/http"
	"net/netip"
	"syscall"
	"time"

	"github.com/godbus/dbus/v5"
)

const hotspotPort = 80

func parseHotspotAddress(value string) (netip.Addr, error) {
	addr, err := netip.ParseAddr(value)
	if err != nil || !addr.Is4() || addr.IsLoopback() || addr.IsUnspecified() ||
		addr.IsMulticast() || addr == netip.AddrFrom4([4]byte{255, 255, 255, 255}) {
		return netip.Addr{}, errors.New("--hotspot-address requires a concrete unicast IPv4 address other than loopback")
	}
	return addr, nil
}

func main() {
	hotspot := flag.String("hotspot-address", "", "required IPv4 address of the protected Wi-Fi hotspot")
	flag.Parse()
	if flag.NArg() != 0 {
		log.Fatal("Unexpected positional arguments")
	}
	addr, err := parseHotspotAddress(*hotspot)
	if err != nil {
		log.Fatal(err)
	}
	listener, err := listenOnHotspot(addr)
	if err != nil {
		log.Fatal("Cannot bind the Wi-Fi setup address: ", err)
	}
	defer listener.Close()
	bus, err := dbus.ConnectSystemBus()
	if err != nil {
		log.Fatal("Cannot connect to the system bus")
	}
	defer bus.Close()

	server := &http.Server{
		Handler:           &portal{hotspotAddress: addr, network: deviceCore{bus.Object(busName, networkPath)}},
		ReadHeaderTimeout: 3 * time.Second,
		ReadTimeout:       5 * time.Second,
		WriteTimeout:      12 * time.Second,
		IdleTimeout:       15 * time.Second,
		MaxHeaderBytes:    4096,
	}
	// A dead bus connection requires a fresh portal process.
	go func() {
		<-bus.Context().Done()
		_ = server.Close()
	}()
	_ = server.Serve(listener)
	log.Fatal("Wi-Fi setup server stopped")
}

func listenOnHotspot(addr netip.Addr) (net.Listener, error) {
	bind := netip.AddrPortFrom(addr, hotspotPort).String()
	for {
		listener, err := net.Listen("tcp4", bind)
		if !errors.Is(err, syscall.EADDRNOTAVAIL) {
			return listener, err
		}
		time.Sleep(2 * time.Second)
	}
}
