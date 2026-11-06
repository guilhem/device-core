package main

import "testing"

func TestParseHotspotAddress(t *testing.T) {
	for _, value := range []string{testHost, "0.0.0.1", "126.255.255.255", "128.0.0.1", "223.255.255.255", "240.0.0.1", "255.255.255.254"} {
		if addr, err := parseHotspotAddress(value); err != nil || addr.String() != value {
			t.Fatalf("valid address %q: got %v, %v", value, addr, err)
		}
	}
	for _, value := range []string{
		"", "0.0.0.0", "127.0.0.1", "127.2.3.4", "255.255.255.255", "224.0.0.1", "239.255.255.255",
		"::1", "::ffff:10.77.0.1", "10.77.0.1:80", "10.77.0.1/24", "example.com",
		"10.077.0.1", "10.77.00.1", "10.77.0.01", "01.2.3.4", "256.0.0.1", "10.77.0", "10.77.0.1.2",
		"-1.2.3.4", "10.77.0.1\n", " 10.77.0.1", "10.77.0.1\nlocal=/#/",
	} {
		t.Run(value, func(t *testing.T) {
			if addr, err := parseHotspotAddress(value); err == nil {
				t.Fatalf("invalid address accepted: %v", addr)
			}
		})
	}
}
