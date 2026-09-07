use super::*;

fn pair() -> (Arc<Channel>, Arc<Channel>) {
    let (handshake, hello) = initiate().unwrap();
    let (server, reply) = respond(&hello).unwrap();
    (finish(handshake, &reply).unwrap(), server)
}

fn head(size: usize) -> RequestHead {
    RequestHead {
        method: "POST".into(),
        url: "/api/command?secret=private".into(),
        headers: vec![("Content-Type".into(), "application/json".into())],
        body_length: size,
    }
}

#[test]
fn handshake_direction_nonce_and_tampering() {
    let (client, server) = pair();
    let packet = client.seal(0, 0, HEADER, b"password").unwrap();
    assert_eq!(server.open(0, 0, &packet[2..]).unwrap(), b"\0password");
    assert!(client.open(0, 0, &packet[2..]).is_err());
    assert!(server.open(1, 0, &packet[2..]).is_err());
    assert!(server.open(0, 1, &packet[2..]).is_err());
    assert!(pair().1.open(0, 0, &packet[2..]).is_err());
    for index in 2..packet.len() {
        let mut changed = packet.clone();
        changed[index] ^= 1;
        assert!(server.open(0, 0, &changed[2..]).is_err());
    }
    let reply = server.seal(0, 0, HEADER, b"response").unwrap();
    assert_eq!(client.open(0, 0, &reply[2..]).unwrap(), b"\0response");
    assert!(respond(&[0; 31]).is_err());
    let (handshake, _) = initiate().unwrap();
    assert!(finish(handshake, &[0; 48]).is_err());
}

#[test]
fn binary_roundtrip_and_replay_window() {
    let (client, server) = pair();
    let body: Vec<u8> = (0..100_000).map(|n| n as u8).collect();
    for number in [3, 1, 2, 0, 2048, 2047] {
        let encoded =
            encode_request(Arc::clone(&client), number, &head(body.len()), &body).unwrap();
        let (decoded_head, decoded) =
            decode_request(Arc::clone(&server), number, &encoded).unwrap();
        assert_eq!(decoded_head.url, head(0).url);
        assert_eq!(decoded, body);
        assert!(decode_request(Arc::clone(&server), number, &encoded).is_err());
    }
    let old = encode_request(client, 4, &head(body.len()), &body).unwrap();
    assert!(decode_request(server, 4, &old).is_err());
}

#[test]
fn invalid_request_does_not_consume_number() {
    let (client, server) = pair();
    let valid = encode_request(client, 0, &head(5), b"hello").unwrap();
    let mut changed = valid.clone();
    changed[20] ^= 1;
    assert!(decode_request(Arc::clone(&server), 0, &changed).is_err());
    assert!(decode_request(server, 0, &valid).is_ok());
}

#[test]
fn stream_requires_authenticated_end_and_exact_length() {
    let (client, server) = pair();
    let valid = encode_request(Arc::clone(&client), 0, &head(5), b"hello").unwrap();
    for length in 0..valid.len() {
        assert!(
            decode_request(Arc::clone(&server), 0, &valid[..length]).is_err(),
            "accepted truncation at {length}"
        );
    }
    let mut extra = valid.clone();
    extra.push(0);
    assert!(decode_request(Arc::clone(&server), 0, &extra).is_err());
    for expected in [0, 4, 6] {
        let mut reader = EncryptReader::new(
            Arc::clone(&client),
            1 + expected as u32,
            &serde_json::to_vec(&head(expected)).unwrap(),
            Cursor::new(b"hello"),
        )
        .unwrap();
        let mut encoded = Vec::new();
        reader.read_to_end(&mut encoded).unwrap();
        assert!(decode_request(Arc::clone(&server), 1 + expected as u32, &encoded).is_err());
    }
    assert!(decode_request(server, 0, &valid).is_ok());
}

#[test]
fn record_order_and_record_bounds() {
    let (client, server) = pair();
    for kinds in [
        [DATA, DATA, END],
        [HEADER, HEADER, END],
        [HEADER, END, DATA],
    ] {
        let mut encoded = Vec::new();
        for (block, kind) in kinds.into_iter().enumerate() {
            let payload = if kind == HEADER {
                serde_json::to_vec(&head(1)).unwrap()
            } else if kind == DATA {
                vec![1]
            } else {
                vec![]
            };
            encoded.extend(client.seal(0, block as u32, kind, &payload).unwrap());
        }
        assert!(decode_request(Arc::clone(&server), 0, &encoded).is_err());
    }
    assert!(client.seal(u32::MAX, 0, HEADER, &[]).is_err());
    assert!(client.seal(0, u32::MAX, DATA, &[]).is_err());
    assert!(client.seal(0, 0, END, &[1]).is_err());
    assert!(client.seal(0, 0, DATA, &vec![0; CHUNK_BYTES + 1]).is_err());
    assert!(read_record(&mut Cursor::new([0, 16]), &server, 0, 0).is_err());
    client.next_request.store(u32::MAX - 1, Ordering::Relaxed);
    assert_eq!(client.next_request().unwrap(), u32::MAX - 1);
    assert!(client.next_request().is_err());
    assert!(server.accept_request(u32::MAX).is_err());
}

#[test]
fn bounded_request_and_small_io_buffers() {
    let (client, server) = pair();
    assert!(encode_request(Arc::clone(&client), 0, &head(1), &[]).is_err());
    assert!(
        encode_request(
            Arc::clone(&client),
            0,
            &head(MAX_REQUEST_BYTES + 1),
            &vec![0; MAX_REQUEST_BYTES + 1]
        )
        .is_err()
    );
    let body: Vec<u8> = (0..CHUNK_BYTES * 3).map(|n| n as u8).collect();
    let mut encoded = EncryptReader::new(client, 0, b"metadata", Cursor::new(&body)).unwrap();
    let mut wire = Vec::new();
    let mut small = [0; 7];
    loop {
        let n = encoded.read(&mut small).unwrap();
        if n == 0 {
            break;
        }
        wire.extend_from_slice(&small[..n]);
    }
    let (metadata, mut decoded) = DecryptReader::new(Cursor::new(wire), server, 0).unwrap();
    assert_eq!(metadata, b"metadata");
    decoded.set_expected_length(Some(body.len() as u64));
    let mut received = Vec::new();
    loop {
        let n = decoded.read(&mut small).unwrap();
        if n == 0 {
            break;
        }
        received.extend_from_slice(&small[..n]);
    }
    assert_eq!(received, body);
}

#[test]
fn concurrent_unique_numbers_and_single_dispatch() {
    let (client, server) = pair();
    let workers: Vec<_> = (0..32)
        .map(|_| {
            let client = Arc::clone(&client);
            let server = Arc::clone(&server);
            std::thread::spawn(move || {
                let n = client.next_request().unwrap();
                let encoded = encode_request(client, n, &head(0), &[]).unwrap();
                decode_request(Arc::clone(&server), n, &encoded).unwrap();
                assert!(decode_request(server, n, &encoded).is_err());
                n
            })
        })
        .collect();
    let numbers: BTreeSet<_> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    assert_eq!(numbers.len(), 32);
}
