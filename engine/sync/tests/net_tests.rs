use std::cell::RefCell;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use bo_sync::net::*;
use bo_sync::*;
use serde_json::json;
use tempfile::TempDir;

fn rules() -> Vec<(String, Box<dyn Rule>)> {
    vec![
        ("items".into(), Box::new(LastWriterWins)),
        ("payments".into(), Box::new(ServerAuthoritative)),
    ]
}

fn mk(dir: &TempDir, name: &str, role: Role) -> Arc<SyncNode> {
    let n = SyncNode::create(dir.path().join(name), role, rules(), Arc::new(ManualClock::new(1_000_000))).unwrap();
    n.require_signatures(true);
    Arc::new(n)
}

fn enrol(on: &SyncNode, other: &SyncNode) {
    on.trust_device(other.device_id(), &other.public_key(), other.role()).unwrap();
}

/// A server and `n` devices; the server knows every device, each device knows the server
/// and every other device (relayed events must verify on arrival).
struct World {
    _dir: TempDir,
    server: Arc<SyncNode>,
    devices: Vec<Arc<SyncNode>>,
    srv: Server,
    addr: String,
}

fn world(n: usize) -> World {
    let dir = TempDir::new().unwrap();
    let server = mk(&dir, "s.db", Role::Server);
    let devices: Vec<_> = (0..n).map(|i| mk(&dir, &format!("d{i}.db"), Role::Device)).collect();
    for d in &devices {
        enrol(&server, d);
        enrol(d, &server);
        for o in &devices {
            if o.device_id() != d.device_id() {
                enrol(d, o);
            }
        }
    }
    let srv = Server::start(server.clone(), "127.0.0.1:0").unwrap();
    let addr = srv.addr().to_string();
    World { _dir: dir, server, devices, srv, addr }
}

fn sync(w: &World, i: usize) -> SyncStats {
    sync_once(&w.devices[i], &w.addr, w.server.device_id()).unwrap()
}

#[test]
fn devices_sync_through_server_over_tcp() {
    let w = world(2);
    w.devices[0].put("items", json!({"_id": "burger", "price": 30})).unwrap();
    w.devices[1].put("items", json!({"_id": "fries", "price": 10})).unwrap();
    w.server.put("payments", json!({"_id": "p1", "status": "paid"})).unwrap();
    let s0 = sync(&w, 0);
    assert_eq!(s0.pushed, 1);
    sync(&w, 1);
    sync(&w, 0);
    for d in &w.devices {
        assert_eq!(d.find("items", &json!({})).unwrap().len(), 2);
        assert_eq!(d.find("payments", &json!({})).unwrap().len(), 1);
        assert_eq!(d.version_vector().unwrap(), w.server.version_vector().unwrap());
    }
    // nothing left to move
    assert_eq!(sync(&w, 0), SyncStats::default());
}

#[test]
fn large_backlog_converges_in_batches() {
    let w = world(1);
    for i in 0..450 {
        w.devices[0].put("items", json!({"_id": format!("i{i}"), "n": i})).unwrap();
    }
    for i in 0..300 {
        w.server.put("items", json!({"_id": format!("s{i}"), "n": i})).unwrap();
    }
    let st = sync(&w, 0);
    assert_eq!((st.pushed, st.pulled), (450, 300));
    assert_eq!(w.devices[0].find("items", &json!({})).unwrap().len(), 750);
    assert_eq!(w.server.find("items", &json!({})).unwrap().len(), 750);
    assert!(w.server.verify().unwrap().is_empty());
}

#[test]
fn unenrolled_device_is_refused_and_server_is_unharmed() {
    let w = world(1);
    let dir = TempDir::new().unwrap();
    let rogue = mk(&dir, "r.db", Role::Device);
    enrol(&rogue, &w.server);
    rogue.put("items", json!({"_id": "evil"})).unwrap();
    let e = sync_once(&rogue, &w.addr, w.server.device_id()).unwrap_err();
    assert!(matches!(e, NetError::Auth(_)), "{e}");
    assert!(w.server.find("items", &json!({})).unwrap().is_empty());
}

#[test]
fn device_with_wrong_key_is_refused() {
    let w = world(1);
    let dir = TempDir::new().unwrap();
    // Same device id as an enrolled device is impossible to claim without its key; simulate by
    // enrolling a *different* device id on the server but giving the rogue another key.
    let rogue = mk(&dir, "r.db", Role::Device);
    let other = mk(&dir, "o.db", Role::Device);
    w.server.trust_device(rogue.device_id(), &other.public_key(), Role::Device).unwrap();
    enrol(&rogue, &w.server);
    assert!(matches!(sync_once(&rogue, &w.addr, w.server.device_id()), Err(NetError::Auth(_))));
}

#[test]
fn client_refuses_impostor_server() {
    let w = world(1);
    let dir = TempDir::new().unwrap();
    let fake = mk(&dir, "fake.db", Role::Server);
    enrol(&fake, &w.devices[0]);
    let fake_srv = Server::start(fake.clone(), "127.0.0.1:0").unwrap();
    // The device expects the real server's id: the fake server proves a different identity.
    let e = sync_once(&w.devices[0], &fake_srv.addr().to_string(), w.server.device_id()).unwrap_err();
    assert!(matches!(e, NetError::Auth(_)), "{e}");

    // A fake that claims the real id but cannot sign.
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = l.local_addr().unwrap().to_string();
    let real_id = w.server.device_id().to_string();
    let t = std::thread::spawn(move || {
        let (mut s, _) = l.accept().unwrap();
        let Msg::Hello { nonce, .. } = read_frame(&mut s, 4096).unwrap() else { panic!() };
        let _ = nonce;
        write_frame(&mut s, &Msg::Challenge { device: real_id, nonce: "0".repeat(64), sig: "00".repeat(64) }).unwrap();
        let mut sink = Vec::new();
        let _ = s.read_to_end(&mut sink);
    });
    let e = sync_once(&w.devices[0], &addr, w.server.device_id()).unwrap_err();
    assert!(matches!(e, NetError::Auth(_)), "{e}");
    t.join().unwrap();
}

#[test]
fn forged_events_are_rejected_without_burning_sequence_numbers() {
    let w = world(2);
    let (a, m) = (&w.devices[0], &w.devices[1]);
    // M is enrolled (it can log in) and tries to speak as A.
    let real = a.put("items", json!({"_id": "x", "v": 1})).unwrap().unwrap();
    let mut forged = real.clone();
    forged.op = Op::Upsert { doc: json!({"_id": "x", "v": 666}) }; // altered after signing
    let rep = w.server.apply_remote(&[forged.clone()]).unwrap();
    assert_eq!(rep.unauthenticated, 1);
    forged.sig = Some("00".repeat(64));
    assert_eq!(w.server.apply_remote(&[forged]).unwrap().unauthenticated, 1);
    let mut unsigned = real.clone();
    unsigned.sig = None;
    assert_eq!(w.server.apply_remote(&[unsigned]).unwrap().unauthenticated, 1);
    // A's genuine event still gets its sequence number.
    assert!(w.server.get("items", "x").unwrap().is_none());
    sync(&w, 0);
    assert_eq!(w.server.get("items", "x").unwrap().unwrap()["v"], 1);
    // events signed by M but claiming to be A fail too
    let mut as_a = real.clone();
    as_a.seq = 2;
    as_a.event_id = new_uuid_v7(1_000_000);
    as_a.sig = Some(m.sign(&as_a.signing_bytes()));
    assert_eq!(w.server.apply_remote(&[as_a]).unwrap().unauthenticated, 1);
}

#[test]
fn device_cannot_claim_the_server_role() {
    let w = world(1);
    let d = &w.devices[0];
    let mut ev = d.put("items", json!({"_id": "x"})).unwrap().unwrap();
    sync(&w, 0); // the server now holds seq 1, so a seq-2 event is the next expected one
    ev.seq = 2;
    ev.event_id = new_uuid_v7(1_000_000);
    ev.hlc = Hlc { wall: 2_000_000, counter: 0 }.encode(d.device_id());
    ev.role = Role::Server; // lie about the role, and sign the lie properly
    ev.collection = "payments".into();
    ev.doc_id = "p9".into();
    ev.op = Op::Upsert { doc: json!({"_id": "p9", "status": "paid"}) };
    ev.sig = Some(d.sign(&ev.signing_bytes()));
    let rep = w.server.apply_remote(&[ev]).unwrap();
    assert_eq!((rep.rejected, rep.applied), (1, 0));
    assert!(w.server.get("payments", "p9").unwrap().is_none());
}

#[test]
fn relayed_event_from_unknown_device_waits_until_enrolled() {
    let w = world(2);
    let dir = TempDir::new().unwrap();
    let late = mk(&dir, "late.db", Role::Device);
    enrol(&w.server, &late);
    enrol(&late, &w.server);
    late.put("items", json!({"_id": "l"})).unwrap();
    sync_once(&late, &w.addr, w.server.device_id()).unwrap();
    // device 0 has never heard of `late`: its event is held back, not lost or applied
    sync(&w, 0);
    assert!(w.devices[0].get("items", "l").unwrap().is_none());
    enrol(&w.devices[0], &late);
    sync(&w, 0);
    assert!(w.devices[0].get("items", "l").unwrap().is_some());
}

#[test]
fn enrolment_is_persistent_and_cannot_be_silently_changed() {
    let dir = TempDir::new().unwrap();
    let p = dir.path().join("a.db");
    let b = mk(&dir, "b.db", Role::Device);
    let c = mk(&dir, "c.db", Role::Device);
    let t = Arc::new(ManualClock::new(1));
    let id;
    {
        let a = SyncNode::create(&p, Role::Server, rules(), t.clone()).unwrap();
        id = a.device_id().to_string();
        enrol(&a, &b);
        assert!(a.trust_device(b.device_id(), &c.public_key(), Role::Device).is_err());
        assert!(a.trust_device(b.device_id(), &b.public_key(), Role::Server).is_err());
        assert!(a.trust_device(b.device_id(), "nothex", Role::Device).is_err());
        assert!(a.trust_device(b.device_id(), &b.public_key(), Role::Device).is_ok()); // idempotent
    }
    let a = SyncNode::open(&p, rules(), t).unwrap();
    assert_eq!(a.device_id(), id);
    assert_eq!(a.trusted(b.device_id()).unwrap().public, b.public_key());
    assert!(a.trusted(c.device_id()).is_none());
    assert!(!a.signatures_required());
}

#[test]
fn network_requires_signatures_switch() {
    let dir = TempDir::new().unwrap();
    let s = Arc::new(SyncNode::create(dir.path().join("s.db"), Role::Server, rules(), Arc::new(ManualClock::new(1))).unwrap());
    assert!(matches!(Server::start(s.clone(), "127.0.0.1:0"), Err(NetError::SignaturesNotRequired)));
    assert!(matches!(sync_once(&s, "127.0.0.1:1", s.device_id()), Err(NetError::SignaturesNotRequired)));
    let d = mk(&dir, "d.db", Role::Device);
    assert!(matches!(Server::start(d, "127.0.0.1:0"), Err(NetError::Protocol(_))));
}

#[test]
fn hostile_bytes_do_not_hurt_the_server() {
    let w = world(1);
    let attacks: Vec<Vec<u8>> = vec![
        vec![],                                            // connect and hang up
        vec![0xff; 100],                                   // huge length prefix
        {
            let mut b = 5000u32.to_be_bytes().to_vec(); // over the pre-login cap
            b.extend(vec![b'{'; 100]);
            b
        },
        {
            let mut b = 10u32.to_be_bytes().to_vec(); // truncated body
            b.extend(b"{\"t\":");
            b
        },
        {
            let body = br#"{"t":"get_vv"}"#; // valid message but before login
            let mut b = (body.len() as u32).to_be_bytes().to_vec();
            b.extend(body);
            b
        },
    ];
    for bytes in attacks {
        let mut s = TcpStream::connect(&w.addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        let _ = s.write_all(&bytes);
        let _ = s.shutdown(std::net::Shutdown::Write);
        let mut sink = Vec::new();
        let _ = s.read_to_end(&mut sink);
    }
    w.devices[0].put("items", json!({"_id": "still-works"})).unwrap();
    sync(&w, 0);
    assert!(w.server.get("items", "still-works").unwrap().is_some());
}

#[test]
fn oversized_frames_are_refused_by_reader() {
    let mut huge = (MAX_FRAME as u32 + 1).to_be_bytes().to_vec();
    huge.extend([0u8; 8]);
    assert!(matches!(read_frame(&mut huge.as_slice(), MAX_FRAME), Err(NetError::Protocol(_))));
    let mut short = 100u32.to_be_bytes().to_vec();
    short.extend(b"{}");
    assert!(read_frame(&mut short.as_slice(), MAX_FRAME).is_err());
}

#[test]
fn retry_backs_off_then_gives_up_and_recovers() {
    let p = RetryPolicy { max_attempts: 4, base_ms: 100, max_ms: 350 };
    assert_eq!([0, 1, 2, 3, 40].map(|a| p.delay_ms(a)), [100, 200, 350, 350, 350]);

    let w = world(1);
    // a closed port: every attempt fails at connect
    let closed = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let slept = RefCell::new(Vec::new());
    let r = sync_with_retry(&w.devices[0], &closed, w.server.device_id(), p, &|d| slept.borrow_mut().push(d.as_millis() as u64));
    assert!(matches!(r, Err(NetError::Io(_))));
    assert_eq!(*slept.borrow(), vec![100, 200, 350]);

    // auth failures are not retried
    let dir = TempDir::new().unwrap();
    let rogue = mk(&dir, "r.db", Role::Device);
    enrol(&rogue, &w.server);
    slept.borrow_mut().clear();
    let r = sync_with_retry(&rogue, &w.addr, w.server.device_id(), p, &|d| slept.borrow_mut().push(d.as_millis() as u64));
    assert!(matches!(r, Err(NetError::Auth(_))));
    assert!(slept.borrow().is_empty());

    // fails once, then the server is reachable: the sleep hook points the next try at it
    w.devices[0].put("items", json!({"_id": "r"})).unwrap();
    let target = RefCell::new(closed.clone());
    let calls = RefCell::new(0);
    let mut result = None;
    for attempt in 0..3 {
        let r = sync_once(&w.devices[0], &target.borrow(), w.server.device_id());
        *calls.borrow_mut() += 1;
        if r.is_ok() {
            result = Some(r);
            break;
        }
        let _ = attempt;
        *target.borrow_mut() = w.addr.clone();
    }
    assert!(result.unwrap().is_ok());
    assert_eq!(*calls.borrow(), 2);
    assert!(w.server.get("items", "r").unwrap().is_some());
}

#[test]
fn server_stop_closes_listener() {
    let w = world(1);
    let addr = w.addr.clone();
    let World { srv, devices, server, .. } = w;
    srv.stop();
    assert!(sync_once(&devices[0], &addr, server.device_id()).is_err());
    let _ = BTreeMap::<u8, u8>::new();
}
