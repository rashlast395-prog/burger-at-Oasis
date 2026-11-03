# bo-server and bo-device

`bo-server` is the remote sync node (run it on the Linux server). `bo-device` is a small
command line tool for provisioning a device, syncing, and smoke tests. Real tills and phones
will embed `bo-sync` instead; this is the same code path.

**Experimental. Do not use for real orders or payments yet.** See "Not done" below.

## Set up

On the server (names/IPs must be what devices will connect to; the first is used for TLS):

```
bo-server init /var/lib/bo-server --host sync.example.com --listen 0.0.0.0:7443
```

This creates the database, the server identity, and a private certificate authority. It prints
the CA fingerprint. **Move `tls/ca.key` somewhere safe and offline** - the server does not
need it, and anyone holding it can impersonate the server to your devices.

For each device, copy `server-identity.json` over a channel you trust, then:

```
bo-device init ./till1 --server-identity server-identity.json   # prints the device id and key
bo-device show ./till1                                           # compare the CA fingerprint
```

Back on the server, with the server **stopped**:

```
bo-server enroll /var/lib/bo-server --device <id> --public <key>
bo-server run /var/lib/bo-server
```

A device syncs with `bo-device sync ./till1` (retries with backoff if the server is down).

## Running as a service (systemd)

```
[Unit]
Description=Burger Oasis sync server
After=network-online.target

[Service]
ExecStart=/usr/local/bin/bo-server run /var/lib/bo-server
User=bo-server
Restart=on-failure
# SIGTERM shuts down cleanly

[Install]
WantedBy=multi-user.target
```

Keep `/var/lib/bo-server` readable only by that user (`chmod 700`). It holds the server's
private keys, and `node.db` (and any backup of it) contains the signing key.

## Security model

* TLS 1.3, trusting only your own CA (no public CAs). The device refuses any other certificate.
* Inside TLS, device and server prove their identity with Ed25519 keys; only enrolled devices
  can connect, and every event is signed by its originating device.
* **Antivirus warning:** some products (for example Avast's Web/Mail Shield) intercept TLS on
  every port and substitute their own certificate. A pinned device correctly refuses that and
  cannot sync. On such machines exclude the sync address from HTTPS scanning or disable it.

## Not done

* No device revocation or re-keying; no automatic certificate renewal.
* `enroll` needs the server stopped (the database is single-process).
* No backups, metrics or log shipping from the server yet; no rate limiting beyond a
  64-connection cap.
* Orders and inventory follow the rules in `engine/domain` (state machine, stock ledger,
  recipes; the server reconciles stock every 30 s).
* Payments are an append-only ledger: tills record cash (offline too); only the server may
  record card or mobile money. The server prints orders whose money needs attention (unpaid
  completed orders, cancelled orders still holding money, over-refunds) every 30 s and never
  moves money itself. **Paystack is not connected**: verifying payments with Paystack
  needs the restaurant's account and secret key, and is the next step for online payments.
* Business defaults approved by the owner on 2026-11-02: deduct stock at `accepted`, allow
  overselling (flagged), no editing a placed order, cancel outranks in-progress statuses,
  completed outranks cancel.
* Tested on Windows only; the Linux build has not been run.

## Checking it works

```
powershell -File engine/server/tests/e2e.ps1
```

drives the real programs through provisioning, enrolment, the database lock, a running server,
sync, and the unreachable-server path. It reports a sync blocked by TLS interception as
SKIPPED, never as passed.
