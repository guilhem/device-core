package main

import "testing"

func TestParseHotspotAddress(t *testing.T) {
	if addr, err := parseHotspotAddress(testHost); err != nil || addr != testHotspot {
		t.Fatalf("valid address: got %v, %v", addr, err)
	}
	for _, value := range []string{
		"", "0.0.0.0", "127.2.3.4", "255.255.255.255", "224.0.0.1",
		"::1", "::ffff:10.77.0.1", "10.77.0.1:80", "10.77.0.1/24", "example.com",
	} {
		t.Run(value, func(t *testing.T) {
			if addr, err := parseHotspotAddress(value); err == nil {
				t.Fatalf("invalid address accepted: %v", addr)
			}
		})
	}
}
