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
	reserve(context.Context) (string, error)
	authorized(context.Context, string) (bool, error)
	connect(context.Context, candidate, string) (uint64, error)
	release(context.Context, string) error
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

func (d deviceCore) reserve(ctx context.Context) (token string, err error) {
	err = d.object.CallWithContext(ctx, networkInterface+".Reserve", 0, "").Store(&token)
	return
}

func (d deviceCore) authorized(ctx context.Context, token string) (ok bool, err error) {
	err = d.object.CallWithContext(ctx, networkInterface+".Authorized", 0, token).Store(&ok)
	return
}

func (d deviceCore) connect(ctx context.Context, c candidate, token string) (id uint64, err error) {
	err = d.object.CallWithContext(ctx, networkInterface+".Connect", 0, c.ssid,
		c.security, c.password, c.uuid, token).Store(&id)
	return
}

func (d deviceCore) release(ctx context.Context, token string) error {
	err := d.object.CallWithContext(ctx, networkInterface+".Release", 0, token).Err
	if reply, ok := err.(dbus.Error); ok && len(reply.Body) == 1 && reply.Body[0] == "invalid-reservation" {
		switch reply.Name {
		case "org.freedesktop.DBus.Error.Failed", "org.freedesktop.DBus.Error.AccessDenied":
			// The controller confirms this token no longer holds a reservation.
			return nil
		}
	}
	return err
}
