package main

import (
	"context"

	"github.com/godbus/dbus/v5"
)

const (
	busName          = "io.github.guilhem.DeviceCore1"
	networkInterface = busName + ".Network"
	networkPath      = "/io/github/guilhem/DeviceCore1/Network"
)

// D-Bus structures must keep the wire field order.
type status struct {
	Mode, Generation string
	Ready            bool
	Address          string
	SSID             []byte
	ProfileUUID      string
	AttemptID        uint64
	Phase, Error     string
}

type accessPoint struct {
	SSID     []byte
	Strength uint8
	Security string
}

type profile struct {
	UUID string
	SSID []byte
}

type candidate struct {
	ssid                     []byte
	security, password, uuid string
}

type networkClient interface {
	status(context.Context) (status, error)
	networks(context.Context) ([]accessPoint, error)
	profiles(context.Context) ([]profile, error)
	scan(context.Context) error
	connect(context.Context, candidate) (uint64, error)
}

type deviceCore struct{ object dbus.BusObject }

func (d deviceCore) property(ctx context.Context, name string, dst any) error {
	var value dbus.Variant
	if err := d.object.CallWithContext(ctx, "org.freedesktop.DBus.Properties.Get", 0,
		networkInterface, name).Store(&value); err != nil {
		return err
	}
	return value.Store(dst)
}

func (d deviceCore) status(ctx context.Context) (s status, err error) {
	err = d.property(ctx, "Status", &s)
	return
}

func (d deviceCore) networks(ctx context.Context) (n []accessPoint, err error) {
	err = d.property(ctx, "Networks", &n)
	return
}

func (d deviceCore) profiles(ctx context.Context) (p []profile, err error) {
	err = d.property(ctx, "Profiles", &p)
	return
}

func (d deviceCore) scan(ctx context.Context) error {
	return d.object.CallWithContext(ctx, networkInterface+".Scan", 0).Err
}

func (d deviceCore) connect(ctx context.Context, c candidate) (id uint64, err error) {
	err = d.object.CallWithContext(ctx, networkInterface+".ConnectHotspot", 0, c.ssid,
		c.security, c.password, c.uuid).Store(&id)
	return
}
