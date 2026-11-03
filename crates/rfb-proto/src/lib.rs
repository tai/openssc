//! Apple Screen Sharing protocol (RFB 003.889 + Apple extensions).
//!
//! Verified against macOS 12.7.6 / screensharingd 3.9.8. See docs/protocol/.

use aes::cipher::generic_array::GenericArray;
use aes::cipher::{BlockDecrypt, BlockEncrypt, KeyInit};
use aes::Aes128;
use anyhow::{bail, Context, Result};
use num_bigint::BigUint;
use rand::RngCore;
use sha1::Sha1;
use sha2::{Digest, Sha256, Sha512};
use std::io::{Read, Write};
use std::net::TcpStream;

pub const VIEWER_PRELUDE: &[u8] = &[
    0x21, 0x00, 0x00, 0x3e, 0x00, 0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x03, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0d, 0x00, 0x00, 0x00, 0x07,
    0x00, 0x00, 0x00, 0x08, 0xb0, 0x00, 0x08, 0x03, 0x90, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x12, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x01, 0x00,
    0x00, 0x00, 0x01, 0x0a, 0x00, 0x00, 0x01,
];

pub const SET_ENCRYPTION_2: &[u8] = &[0x12, 0x00, 0x00, 0x02, 0x00, 0x01, 0x00, 0x00];

pub const SET_ENCODINGS: &[u8] = &[
    0x02, 0x00, 0x00, 0x0a, 0x00, 0x00, 0x00, 0x06, 0x00, 0x00, 0x00, 0x10, 0xff, 0xff, 0xff,
    0x11, 0x00, 0x00, 0x04, 0x50, 0x00, 0x00, 0x04, 0x4c, 0xff, 0xff, 0xff, 0x21, 0x00, 0x00,
    0x04, 0x4d, 0x00, 0x00, 0x04, 0x51, 0x00, 0x00, 0x04, 0x53, 0x00, 0x00, 0x04, 0x55,
];

#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub width: u16,
    pub height: u16,
    pub pixel_format: [u8; 16],
    pub name: Vec<u8>,
}

fn read_u8(s: &mut TcpStream) -> Result<u8> {
    let mut b = [0u8; 1];
    s.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_u16(s: &mut TcpStream) -> Result<u16> {
    let mut b = [0u8; 2];
    s.read_exact(&mut b)?;
    Ok(u16::from_be_bytes(b))
}

fn read_u32(s: &mut TcpStream) -> Result<u32> {
    let mut b = [0u8; 4];
    s.read_exact(&mut b)?;
    Ok(u32::from_be_bytes(b))
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

fn pad(x: &BigUint, len: usize) -> Vec<u8> {
    let raw = x.to_bytes_be();
    let mut out = vec![0u8; len];
    if raw.len() <= len {
        out[len - raw.len()..].copy_from_slice(&raw);
    }
    out
}

fn sha512(data: &[u8]) -> [u8; 64] {
    let d = Sha512::digest(data);
    let mut out = [0u8; 64];
    out.copy_from_slice(&d);
    out
}

fn der_tlv(data: &[u8], off: &mut usize) -> Result<(u8, Vec<u8>)> {
    if *off + 2 > data.len() {
        bail!("short DER");
    }
    let tag = data[*off];
    *off += 1;
    let mut len = data[*off] as usize;
    *off += 1;
    if len & 0x80 != 0 {
        let n = len & 0x7f;
        if *off + n > data.len() {
            bail!("short DER length");
        }
        len = 0;
        for _ in 0..n {
            len = (len << 8) | data[*off] as usize;
            *off += 1;
        }
    }
    if *off + len > data.len() {
        bail!("short DER value");
    }
    let val = data[*off..*off + len].to_vec();
    *off += len;
    Ok((tag, val))
}

/// Parse an RSA SubjectPublicKeyInfo DER blob into (n, e).
pub fn parse_rsa_spki(der: &[u8]) -> Result<(BigUint, BigUint)> {
    let mut o = 0;
    let (tag, seq) = der_tlv(der, &mut o)?;
    if tag != 0x30 {
        bail!("SPKI is not a sequence");
    }
    let mut so = 0;
    let (tag, _alg) = der_tlv(&seq, &mut so)?;
    if tag != 0x30 {
        bail!("missing algorithm identifier");
    }
    let (tag, bits) = der_tlv(&seq, &mut so)?;
    if tag != 0x03 || bits.is_empty() || bits[0] != 0 {
        bail!("bad RSA bit string");
    }
    let mut bo = 1;
    let (tag, rsa) = der_tlv(&bits, &mut bo)?;
    if tag != 0x30 {
        bail!("RSA key is not a sequence");
    }
    let mut ro = 0;
    let (tag, n) = der_tlv(&rsa, &mut ro)?;
    if tag != 0x02 {
        bail!("RSA modulus is not an integer");
    }
    let (tag, e) = der_tlv(&rsa, &mut ro)?;
    if tag != 0x02 {
        bail!("RSA exponent is not an integer");
    }
    Ok((BigUint::from_bytes_be(&n), BigUint::from_bytes_be(&e)))
}

/// PKCS#1 v1.5 RSA encryption (the identity envelope in auth type 33).
pub fn rsa_encrypt(n: &BigUint, e: &BigUint, msg: &[u8]) -> Result<Vec<u8>> {
    let k = (n.bits() as usize + 7) / 8;
    if msg.len() + 11 > k {
        bail!("message too long for RSA key");
    }
    let ps_len = k - 3 - msg.len();
    let mut em = Vec::with_capacity(k);
    em.push(0x00);
    em.push(0x02);
    let mut ps = vec![0u8; ps_len];
    rand::thread_rng().fill_bytes(&mut ps);
    for b in ps.iter_mut() {
        while *b == 0 {
            *b = rand::thread_rng().next_u32() as u8;
        }
    }
    em.extend_from_slice(&ps);
    em.push(0x00);
    em.extend_from_slice(msg);
    let c = BigUint::from_bytes_be(&em).modpow(e, n);
    let raw = c.to_bytes_be();
    let mut out = vec![0u8; k];
    out[k - raw.len()..].copy_from_slice(&raw);
    Ok(out)
}

fn build_identity(username: &str) -> Vec<u8> {
    let u = username.as_bytes();
    let payload_len = 4 + u.len() + 2 + 1;
    let mut out = Vec::new();
    out.extend_from_slice(&(payload_len as u32).to_be_bytes());
    out.extend_from_slice(&(u.len() as u32).to_be_bytes());
    out.extend_from_slice(u);
    out.extend_from_slice(&[0, 0, 0]);
    out
}

struct Challenge {
    n: Vec<u8>,
    g: BigUint,
    salt: Vec<u8>,
    b: Vec<u8>,
    iterations: u64,
    options: Vec<u8>,
}

fn parse_challenge(payload: &[u8]) -> Result<Challenge> {
    if payload.len() < 10 {
        bail!("short SRP challenge");
    }
    let version = be32(&payload[0..4]);
    let (mut o, _x) = if version == 2 {
        (6usize, be16(&payload[4..6]) as u32)
    } else {
        (5usize, be32(&payload[1..5]))
    };
    let plen = be32(&payload[o..o + 4]) as usize;
    o += 4;
    let inner = &payload[o..o + plen];
    let mut p = 0;
    let _control = inner[p];
    p += 1;
    let ln = be16(&inner[p..p + 2]) as usize;
    p += 2;
    let n = inner[p..p + ln].to_vec();
    p += ln;
    let ln = be16(&inner[p..p + 2]) as usize;
    p += 2;
    let g = BigUint::from_bytes_be(&inner[p..p + ln]);
    p += ln;
    let ln = inner[p] as usize;
    p += 1;
    let salt = inner[p..p + ln].to_vec();
    p += ln;
    let ln = be16(&inner[p..p + 2]) as usize;
    p += 2;
    let b = inner[p..p + ln].to_vec();
    p += ln;
    let iterations = be64(&inner[p..p + 8]);
    p += 8;
    let ln = be16(&inner[p..p + 2]) as usize;
    p += 2;
    let options = inner[p..p + ln].to_vec();
    Ok(Challenge { n, g, salt, b, iterations, options })
}

/// SRP-6a client, verified byte-exact against the real client's M1.
fn srp_client(ch: &Challenge, password: &str) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>)> {
    let nbytes = &ch.n;
    let n = BigUint::from_bytes_be(nbytes);
    let pad_len = nbytes.len();

    let mut p_prime = vec![0u8; 128];
    pbkdf2_sha512(password.as_bytes(), &ch.salt, ch.iterations as u32, &mut p_prime);

    let mut inner = Vec::with_capacity(1 + p_prime.len());
    inner.push(b':');
    inner.extend_from_slice(&p_prime);
    let mut x_input = ch.salt.clone();
    x_input.extend_from_slice(&sha512(&inner));
    let x = BigUint::from_bytes_be(&sha512(&x_input));

    let mut a_bytes = [0u8; 64];
    rand::thread_rng().fill_bytes(&mut a_bytes);
    let a = BigUint::from_bytes_be(&a_bytes);
    let a_pub = ch.g.modpow(&a, &n);
    let a_wire = pad(&a_pub, pad_len);

    let b = BigUint::from_bytes_be(&ch.b);
    let u = BigUint::from_bytes_be(&sha512(&[a_wire.clone(), pad(&b, pad_len)].concat()));

    let mut k_input = pad(&n, pad_len);
    k_input.extend_from_slice(&pad(&ch.g, pad_len));
    let k = BigUint::from_bytes_be(&sha512(&k_input));

    let gx = ch.g.modpow(&x, &n);
    let base = (b + &n - ((k * gx) % &n)) % &n;
    let exp = &a + (&u * &x);
    let s = base.modpow(&exp, &n);
    let kk = sha512(&pad(&s, pad_len));

    let hn = sha512(nbytes);
    let hg = sha512(&pad(&ch.g, pad_len));
    let hi = sha512(b"");
    let mut m1_input = Vec::new();
    for i in 0..64 {
        m1_input.push(hn[i] ^ hg[i]);
    }
    m1_input.extend_from_slice(&hi);
    m1_input.extend_from_slice(&ch.salt);
    m1_input.extend_from_slice(&a_wire);
    m1_input.extend_from_slice(&ch.b);
    m1_input.extend_from_slice(&kk);
    let m1 = sha512(&m1_input).to_vec();

    let mut m2_input = a_wire.clone();
    m2_input.extend_from_slice(&m1);
    m2_input.extend_from_slice(&kk);
    let m2 = sha512(&m2_input).to_vec();

    Ok((a_wire, m1, m2, kk.to_vec()))
}

fn pbkdf2_sha512(password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
    // Minimal PBKDF2-HMAC-SHA512 implementation (RFC 8018).
    use hmac::Mac;
    type HmacSha512 = hmac::Hmac<Sha512>;
    let h_len = 64;
    for (block, chunk) in out.chunks_mut(h_len).enumerate() {
        let mut mac = <HmacSha512 as Mac>::new_from_slice(password).expect("HMAC key");
        mac.update(salt);
        mac.update(&(block as u32 + 1).to_be_bytes());
        let mut u = mac.finalize().into_bytes();
        let mut t = u.clone();
        for _ in 1..iterations {
            let mut mac = <HmacSha512 as Mac>::new_from_slice(password).expect("HMAC key");
            mac.update(&u);
            u = mac.finalize().into_bytes();
            for i in 0..h_len {
                t[i] ^= u[i];
            }
        }
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
}

fn build_packet2(a: &[u8], m1: &[u8], options: &[u8], rnd: &[u8]) -> Vec<u8> {
    let mut inner = Vec::new();
    inner.extend_from_slice(&(a.len() as u16).to_be_bytes());
    inner.extend_from_slice(a);
    inner.push(m1.len() as u8);
    inner.extend_from_slice(m1);
    inner.extend_from_slice(&(options.len() as u16).to_be_bytes());
    inner.extend_from_slice(options);
    inner.push(rnd.len() as u8);
    inner.extend_from_slice(rnd);

    let mut payload = Vec::new();
    payload.extend_from_slice(&0x0100u16.to_be_bytes());
    payload.extend_from_slice(b"RSA1");
    payload.extend_from_slice(&2u16.to_be_bytes());
    payload.extend_from_slice(&((inner.len() + 4) as u16).to_be_bytes());
    payload.extend_from_slice(&0u16.to_be_bytes());
    payload.extend_from_slice(&(inner.len() as u16).to_be_bytes());
    payload.extend_from_slice(&inner);

    let mut out = Vec::new();
    out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    out.extend_from_slice(&payload);
    out
}

fn parse_proof(payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() < 10 {
        bail!("short proof");
    }
    let _version = be32(&payload[0..4]);
    let _x = be16(&payload[4..6]);
    let plen = be32(&payload[6..10]) as usize;
    let inner = &payload[10..10 + plen];
    if inner.is_empty() {
        bail!("empty proof");
    }
    let l = inner[0] as usize;
    Ok(inner[1..1 + l].to_vec())
}

fn aes_ecb_decrypt_block(key: &[u8], block: &[u8]) -> [u8; 16] {
    let cipher = Aes128::new(GenericArray::from_slice(key));
    let mut b = GenericArray::clone_from_slice(&block[..16]);
    cipher.decrypt_block(&mut b);
    let mut out = [0u8; 16];
    out.copy_from_slice(&b);
    out
}

pub struct RecordCipher {
    aes: Aes128,
    iv: [u8; 16],
    seq: u32,
}

impl RecordCipher {
    pub fn new(key: &[u8], iv: &[u8]) -> Self {
        Self {
            aes: Aes128::new(GenericArray::from_slice(key)),
            iv: iv.try_into().expect("16-byte IV"),
            seq: 0,
        }
    }

    pub fn encrypt(&mut self, body: &[u8]) -> Vec<u8> {
        let filler_len = (16 - ((2 + body.len() + 20) % 16)) % 16;
        let mut plain = Vec::with_capacity(2 + body.len() + filler_len + 20);
        plain.extend_from_slice(&(body.len() as u16).to_be_bytes());
        plain.extend_from_slice(body);
        let mut filler = vec![0u8; filler_len];
        rand::thread_rng().fill_bytes(&mut filler);
        plain.extend_from_slice(&filler);
        let mut tag_input = Vec::with_capacity(4 + plain.len());
        tag_input.extend_from_slice(&self.seq.to_be_bytes());
        tag_input.extend_from_slice(&plain);
        plain.extend_from_slice(&Sha1::digest(&tag_input));

        let mut out = Vec::with_capacity(plain.len());
        let mut prev = self.iv;
        for chunk in plain.chunks(16) {
            let mut block = [0u8; 16];
            block.copy_from_slice(chunk);
            for i in 0..16 {
                block[i] ^= prev[i];
            }
            let mut ga = GenericArray::clone_from_slice(&block);
            self.aes.encrypt_block(&mut ga);
            let ct: [u8; 16] = ga.into();
            out.extend_from_slice(&ct);
            prev = ct;
        }
        self.iv = prev;
        self.seq = self.seq.wrapping_add(1);
        out
    }

    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        if ciphertext.is_empty() || ciphertext.len() % 16 != 0 {
            bail!("bad record length {}", ciphertext.len());
        }
        let mut plain = Vec::with_capacity(ciphertext.len());
        let mut prev = self.iv;
        for chunk in ciphertext.chunks(16) {
            let mut ga = GenericArray::clone_from_slice(chunk);
            self.aes.decrypt_block(&mut ga);
            let mut block = [0u8; 16];
            for i in 0..16 {
                block[i] = ga[i] ^ prev[i];
            }
            plain.extend_from_slice(&block);
            prev.copy_from_slice(chunk);
        }
        if plain.len() < 22 {
            bail!("short record plaintext");
        }
        let tag = &plain[plain.len() - 20..];
        let mut tag_input = Vec::with_capacity(4 + plain.len() - 20);
        tag_input.extend_from_slice(&self.seq.to_be_bytes());
        tag_input.extend_from_slice(&plain[..plain.len() - 20]);
        if tag != Sha1::digest(&tag_input).as_slice() {
            bail!("record integrity failure at seq {}", self.seq);
        }
        let body_len = be16(&plain[0..2]) as usize;
        if 2 + body_len > plain.len() - 20 {
            bail!("record body length out of range");
        }
        self.iv = prev;
        self.seq = self.seq.wrapping_add(1);
        Ok(plain[2..2 + body_len].to_vec())
    }
}

pub struct AppleSession {
    pub stream: TcpStream,
    pub info: ServerInfo,
    pub send: RecordCipher,
    pub recv: RecordCipher,
    wrap: [u8; 16],
}

impl AppleSession {
    pub fn connect(host: &str, port: u16, user: &str, password: &str) -> Result<Self> {
        let mut stream = TcpStream::connect((host, port))
            .with_context(|| format!("connect to {host}:{port}"))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(20)))?;

        let mut banner = [0u8; 12];
        stream.read_exact(&mut banner)?;
        if &banner != b"RFB 003.889\n" {
            bail!("unexpected RFB banner: {:?}", String::from_utf8_lossy(&banner));
        }
        stream.write_all(b"RFB 003.889\n")?;

        let count = read_u8(&mut stream)? as usize;
        if count == 0 {
            let n = read_u32(&mut stream)? as usize;
            let mut reason = vec![0u8; n];
            stream.read_exact(&mut reason)?;
            bail!("server refused: {}", String::from_utf8_lossy(&reason));
        }
        let mut types = vec![0u8; count];
        stream.read_exact(&mut types)?;
        if !types.contains(&33) {
            bail!("server does not offer auth type 33: {types:?}");
        }

        // Selector + RSA1 key request in one write (server requirement).
        let mut key_req = Vec::new();
        key_req.push(33u8);
        key_req.extend_from_slice(&10u32.to_be_bytes());
        key_req.extend_from_slice(&0x0100u16.to_be_bytes());
        key_req.extend_from_slice(b"RSA1");
        key_req.extend_from_slice(&0u16.to_be_bytes());
        key_req.extend_from_slice(&0u16.to_be_bytes());
        stream.write_all(&key_req)?;

        let total = read_u32(&mut stream)? as usize;
        let mut kp = vec![0u8; total];
        stream.read_exact(&mut kp)?;
        let _ver = be16(&kp[0..2]);
        let der_len = be32(&kp[2..6]) as usize;
        let (n, e) = parse_rsa_spki(&kp[6..6 + der_len])?;

        let identity = build_identity(user);
        let ct = rsa_encrypt(&n, &e, &identity)?;
        let mut body = vec![0u8; 640];
        body[..ct.len()].copy_from_slice(&ct);
        let mut pkt1 = Vec::new();
        pkt1.extend_from_slice(&((10 + body.len()) as u32).to_be_bytes());
        pkt1.extend_from_slice(&0x0100u16.to_be_bytes());
        pkt1.extend_from_slice(b"RSA1");
        pkt1.extend_from_slice(&2u16.to_be_bytes());
        pkt1.extend_from_slice(&(ct.len() as u16).to_be_bytes());
        pkt1.extend_from_slice(&body);
        stream.write_all(&pkt1)?;

        let total = read_u32(&mut stream)? as usize;
        let mut challenge = vec![0u8; total];
        stream.read_exact(&mut challenge)?;
        let ch = parse_challenge(&challenge)?;
        let (a, m1, m2_expected, k) = srp_client(&ch, password)?;

        let mut rnd = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut rnd);
        stream.write_all(&build_packet2(&a, &m1, &ch.options, &rnd))?;

        let total = read_u32(&mut stream)? as usize;
        let mut proof = vec![0u8; total];
        stream.read_exact(&mut proof)?;
        let m2 = parse_proof(&proof)?;
        if m2 != m2_expected {
            bail!("server M2 verification failed");
        }

        let result = read_u32(&mut stream)?;
        if result != 0 {
            bail!("authentication failed: SecurityResult={result}");
        }

        stream.write_all(&[0xc1])?;
        let mut hdr = [0u8; 24];
        stream.read_exact(&mut hdr)?;
        let width = be16(&hdr[0..2]);
        let height = be16(&hdr[2..4]);
        let mut pixel_format = [0u8; 16];
        pixel_format.copy_from_slice(&hdr[4..20]);
        let name_len = be32(&hdr[20..24]) as usize;
        let mut name = vec![0u8; name_len];
        stream.read_exact(&mut name)?;

        let k256 = Sha256::digest(&k);
        let mut wrap = [0u8; 16];
        wrap.copy_from_slice(&k256[..16]);

        Ok(Self {
            stream,
            info: ServerInfo { width, height, pixel_format, name },
            send: RecordCipher::new(&[0u8; 16], &[0u8; 16]),
            recv: RecordCipher::new(&[0u8; 16], &[0u8; 16]),
            wrap,
        })
    }

    /// Send the cleartext prelude, process the rekey, and activate AES.
    pub fn start_streaming(&mut self) -> Result<()> {
        self.stream.write_all(VIEWER_PRELUDE)?;

        let mut hdr = [0u8; 4];
        self.stream.read_exact(&mut hdr)?;
        if hdr[0] != 0 {
            bail!("expected cleartext FramebufferUpdate, got type {}", hdr[0]);
        }
        let nrects = be16(&hdr[2..4]);
        let mut key = [0u8; 16];
        let mut iv = [0u8; 16];
        for _ in 0..nrects {
            let mut rect = [0u8; 12];
            self.stream.read_exact(&mut rect)?;
            let enc = be32(&rect[8..12]);
            if enc != 0x44f {
                bail!("expected rekey encoding 0x44f, got 0x{enc:x}");
            }
            let mut payload = [0u8; 36];
            self.stream.read_exact(&mut payload)?;
            key = aes_ecb_decrypt_block(&self.wrap, &payload[4..20]);
            iv = aes_ecb_decrypt_block(&self.wrap, &payload[20..36]);
        }
        self.send = RecordCipher::new(&key, &iv);
        self.recv = RecordCipher::new(&key, &iv);
        self.stream.write_all(SET_ENCRYPTION_2)?;
        self.send_message(SET_ENCODINGS)?;
        Ok(())
    }

    pub fn send_message(&mut self, body: &[u8]) -> Result<()> {
        let ct = self.send.encrypt(body);
        let mut framed = Vec::with_capacity(2 + ct.len());
        framed.extend_from_slice(&(ct.len() as u16).to_be_bytes());
        framed.extend_from_slice(&ct);
        self.stream.write_all(&framed)?;
        Ok(())
    }

    pub fn recv_message(&mut self) -> Result<Vec<u8>> {
        let n = read_u16(&mut self.stream)? as usize;
        let mut ct = vec![0u8; n];
        self.stream.read_exact(&mut ct)?;
        self.recv.decrypt(&ct)
    }

    /// Request a full framebuffer and decode the Zlib rectangle to BGRX pixels.
    pub fn request_frame(&mut self) -> Result<Vec<u8>> {
        let mut req = Vec::new();
        req.push(3u8); // FramebufferUpdateRequest
        req.push(0); // non-incremental
        req.extend_from_slice(&0u16.to_be_bytes());
        req.extend_from_slice(&0u16.to_be_bytes());
        req.extend_from_slice(&self.info.width.to_be_bytes());
        req.extend_from_slice(&self.info.height.to_be_bytes());
        self.send_message(&req)?;

        let mut pending: Vec<u8> = Vec::new();
        loop {
            while pending.len() < 4 {
                let body = self.recv_message()?;
                pending.extend_from_slice(&body);
            }
            if pending[0] != 0 {
                let total = match pending[0] {
                    0x14 => 8, // MiscStatus: type, pad, u16 len=4, u16 flags, u16 cmd
                    0x02 => 1, // Bell
                    0x03 => {
                        while pending.len() < 8 {
                            let body = self.recv_message()?;
                            pending.extend_from_slice(&body);
                        }
                        8 + be32(&pending[4..8]) as usize
                    }
                    0x01 => {
                        while pending.len() < 6 {
                            let body = self.recv_message()?;
                            pending.extend_from_slice(&body);
                        }
                        6 + 6 * be16(&pending[4..6]) as usize
                    }
                    0x1f => {
                        while pending.len() < 16 {
                            let body = self.recv_message()?;
                            pending.extend_from_slice(&body);
                        }
                        16 + be32(&pending[12..16]) as usize
                    }
                    t => bail!("unexpected record message type 0x{t:02x}"),
                };
                while pending.len() < total {
                    let body = self.recv_message()?;
                    pending.extend_from_slice(&body);
                }
                pending.drain(..total);
                continue;
            }
            let nrects = be16(&pending[2..4]);
            if nrects == 0 {
                pending.drain(..4);
                continue;
            }
            while pending.len() < 16 {
                let body = self.recv_message()?;
                pending.extend_from_slice(&body);
            }
            let w = be16(&pending[8..10]) as usize;
            let h = be16(&pending[10..12]) as usize;
            let enc = be32(&pending[12..16]);
            match enc {
                6 => {
                    while pending.len() < 20 {
                        let body = self.recv_message()?;
                        pending.extend_from_slice(&body);
                    }
                    let clen = be32(&pending[16..20]) as usize;
                    let mut z = pending[20..].to_vec();
                    pending.clear();
                    while z.len() < clen {
                        let body = self.recv_message()?;
                        z.extend_from_slice(&body);
                    }
                    z.truncate(clen);
                    let expected = w * h * 4;
                    let mut out = vec![0u8; expected];
                    let mut dec = flate2::Decompress::new(true);
                    // The server uses Z_SYNC_FLUSH and does not always emit a
                    // final block, so do not require StreamEnd.
                    let _status = dec
                        .decompress(&z, &mut out, flate2::FlushDecompress::None)
                        .context("zlib framebuffer decode failed")?;
                    let produced = dec.total_out() as usize;
                    if produced != expected {
                        bail!("decoded {produced} bytes, expected {expected}");
                    }
                    out.truncate(produced);
                    return Ok(out);
                }
                0x451 | 0x453 => {
                    while pending.len() < 18 {
                        let body = self.recv_message()?;
                        pending.extend_from_slice(&body);
                    }
                    let plen = be16(&pending[16..18]) as usize;
                    let total = 18 + plen;
                    while pending.len() < total {
                        let body = self.recv_message()?;
                        pending.extend_from_slice(&body);
                    }
                    pending.drain(..total);
                }
                0x450 => {
                    while pending.len() < 24 {
                        let body = self.recv_message()?;
                        pending.extend_from_slice(&body);
                    }
                    let clen = be32(&pending[20..24]) as usize;
                    let total = 24 + clen;
                    while pending.len() < total {
                        let body = self.recv_message()?;
                        pending.extend_from_slice(&body);
                    }
                    pending.drain(..total);
                }
                0 => {
                    let total = 16 + w * h * 4;
                    while pending.len() < total {
                        let body = self.recv_message()?;
                        pending.extend_from_slice(&body);
                    }
                    let pixels = pending[16..total].to_vec();
                    pending.drain(..total);
                    return Ok(pixels);
                }
                other => bail!("unsupported framebuffer encoding 0x{other:x}"),
            }
        }
    }

    pub fn send_gesture(&mut self, body: &[u8]) -> Result<()> {
        self.send_message(body)
    }

    /// Downscale the server-side framebuffer (0x08). Send after the record
    /// layer is active, before the first framebuffer request.
    pub fn set_scale(&mut self, scale: f64) -> Result<()> {
        self.send_message(&scale_factor(scale))
    }
}

#[derive(Debug, Clone)]
pub struct RectUpdate {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
    pub encoding: u32,
    /// BGRX pixels for encoding 0/6; empty for metadata encodings.
    pub pixels: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Update {
    pub rects: Vec<RectUpdate>,
}

pub struct SessionReader {
    pub stream: TcpStream,
    pub recv: RecordCipher,
    pub info: ServerInfo,
    /// RFB Zlib uses one continuous stream for the whole session: the first
    /// rectangle carries the zlib header, later rectangles are continuations.
    zlib: flate2::Decompress,
}

pub struct SessionWriter {
    pub stream: TcpStream,
    pub send: RecordCipher,
    pub info: ServerInfo,
}

impl AppleSession {
    /// Split the authenticated/activated session into independent read/write
    /// halves. Each half owns a cloned socket and one direction of the record
    /// cipher, so a reader thread and a writer thread can run concurrently.
    pub fn split(self) -> Result<(SessionReader, SessionWriter)> {
        let reader_stream = self.stream.try_clone()?;
        let writer_stream = self.stream.try_clone()?;
        reader_stream.set_read_timeout(Some(std::time::Duration::from_secs(20)))?;
        writer_stream.set_write_timeout(Some(std::time::Duration::from_secs(20)))?;
        Ok((
            SessionReader {
                stream: reader_stream,
                recv: self.recv,
                info: self.info.clone(),
                zlib: flate2::Decompress::new(true),
            },
            SessionWriter {
                stream: writer_stream,
                send: self.send,
                info: self.info,
            },
        ))
    }
}

impl SessionWriter {
    pub fn send(&mut self, body: &[u8]) -> Result<()> {
        let ct = self.send.encrypt(body);
        let mut framed = Vec::with_capacity(2 + ct.len());
        framed.extend_from_slice(&(ct.len() as u16).to_be_bytes());
        framed.extend_from_slice(&ct);
        self.stream.write_all(&framed)?;
        Ok(())
    }
}

impl SessionReader {
    fn recv_message(&mut self) -> Result<Vec<u8>> {
        let n = read_u16(&mut self.stream)? as usize;
        let mut ct = vec![0u8; n];
        self.stream.read_exact(&mut ct)?;
        self.recv.decrypt(&ct)
    }

    fn ensure(&mut self, pending: &mut Vec<u8>, need: usize) -> Result<()> {
        while pending.len() < need {
            let body = self.recv_message()?;
            pending.extend_from_slice(&body);
        }
        Ok(())
    }

    fn skip_control(&mut self, pending: &mut Vec<u8>) -> Result<()> {
        let total = match pending[0] {
            0x14 => 8,
            0x02 => 1,
            0x03 => {
                self.ensure(pending, 8)?;
                8 + be32(&pending[4..8]) as usize
            }
            0x01 => {
                self.ensure(pending, 6)?;
                6 + 6 * be16(&pending[4..6]) as usize
            }
            0x1f => {
                self.ensure(pending, 16)?;
                16 + be32(&pending[12..16]) as usize
            }
            t => bail!("unexpected server message type 0x{t:02x}"),
        };
        self.ensure(pending, total)?;
        pending.drain(..total);
        Ok(())
    }

    /// Read one complete FramebufferUpdate (possibly spanning multiple
    /// record-layer records) and return its rectangles.
    pub fn read_update(&mut self) -> Result<Update> {
        let mut pending: Vec<u8> = Vec::new();
        loop {
            self.ensure(&mut pending, 4)?;
            if pending[0] != 0 {
                self.skip_control(&mut pending)?;
                continue;
            }
            let nrects = be16(&pending[2..4]);
            pending.drain(..4);
            if nrects == 0 {
                continue;
            }
            let mut rects = Vec::with_capacity(nrects as usize);
            for _ in 0..nrects {
                self.ensure(&mut pending, 12)?;
                let x = be16(&pending[0..2]);
                let y = be16(&pending[2..4]);
                let w = be16(&pending[4..6]);
                let h = be16(&pending[6..8]);
                let enc = be32(&pending[8..12]);
                match enc {
                    0 => {
                        let n = w as usize * h as usize * 4;
                        self.ensure(&mut pending, 12 + n)?;
                        let pixels = pending[12..12 + n].to_vec();
                        pending.drain(..12 + n);
                        rects.push(RectUpdate { x, y, w, h, encoding: enc, pixels });
                    }
                    6 => {
                        self.ensure(&mut pending, 16)?;
                        let clen = be32(&pending[12..16]) as usize;
                        self.ensure(&mut pending, 16 + clen)?;
                        let z = &pending[16..16 + clen];
                        let expected = w as usize * h as usize * 4;
                        let before = self.zlib.total_out();
                        let mut out = vec![0u8; expected];
                        let _ = self
                            .zlib
                            .decompress(z, &mut out, flate2::FlushDecompress::None)?;
                        let produced = (self.zlib.total_out() - before) as usize;
                        if produced != expected {
                            bail!("zlib rect decoded {produced}, expected {expected}");
                        }
                        pending.drain(..16 + clen);
                        rects.push(RectUpdate { x, y, w, h, encoding: enc, pixels: out });
                    }
                    0x450 => {
                        self.ensure(&mut pending, 20)?;
                        let clen = be32(&pending[16..20]) as usize;
                        self.ensure(&mut pending, 20 + clen)?;
                        pending.drain(..20 + clen);
                        rects.push(RectUpdate { x, y, w, h, encoding: enc, pixels: Vec::new() });
                    }
                    0x451 | 0x453 => {
                        self.ensure(&mut pending, 14)?;
                        let plen = be16(&pending[12..14]) as usize;
                        self.ensure(&mut pending, 14 + plen)?;
                        pending.drain(..14 + plen);
                        rects.push(RectUpdate { x, y, w, h, encoding: enc, pixels: Vec::new() });
                    }
                    other => bail!("unsupported framebuffer encoding 0x{other:x}"),
                }
            }
            return Ok(Update { rects });
        }
    }
}

/// Ask the server to scale the framebuffer (0x08).
pub fn scale_factor(scale: f64) -> Vec<u8> {
    let mut b = Vec::with_capacity(10);
    b.push(0x08);
    b.push(0);
    b.extend_from_slice(&scale.to_be_bytes());
    b
}

/// Switch the server to server-driven framebuffer streaming (0x09).
pub fn auto_framebuffer_update(width: u16, height: u16) -> Vec<u8> {
    auto_framebuffer_update_custom(1, 0xffff_ffff, width, height)
}

pub fn auto_framebuffer_update_custom(
    version: u16,
    selected_screen: u32,
    width: u16,
    height: u16,
) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.push(0x09);
    b.push(0);
    b.extend_from_slice(&version.to_be_bytes());
    b.extend_from_slice(&selected_screen.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&width.to_be_bytes());
    b.extend_from_slice(&height.to_be_bytes());
    b
}

pub fn framebuffer_update_request(width: u16, height: u16, incremental: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(10);
    b.push(3);
    b.push(if incremental { 1 } else { 0 });
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&width.to_be_bytes());
    b.extend_from_slice(&height.to_be_bytes());
    b
}

/// Standard RFB ClientCutText (client -> server clipboard).
pub fn client_cut_text(text: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + text.len());
    b.push(6);
    b.extend_from_slice(&[0, 0, 0]);
    b.extend_from_slice(&(text.len() as u32).to_be_bytes());
    b.extend_from_slice(text);
    b
}

/// Apple AutoPasteboard control. selector 1 = start monitoring, 2 = stop.
pub fn auto_pasteboard(selector: u8) -> Vec<u8> {
    vec![0x15, 0, 0, selector, 0, 0, 0, 0]
}

/// Apple ClipboardFetch (0x0b) after a remote clipboard-changed notification.
pub fn clipboard_fetch() -> Vec<u8> {
    vec![0x0b]
}

pub fn key_event(keysym: u32, down: bool) -> Vec<u8> {
    let mut b = Vec::with_capacity(8);
    b.push(4); // KeyEvent
    b.push(if down { 1 } else { 0 });
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&keysym.to_be_bytes());
    b
}

pub fn pointer_event(mask: u8, x: u16, y: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(6);
    b.push(5); // PointerEvent
    b.push(mask);
    b.extend_from_slice(&x.to_be_bytes());
    b.extend_from_slice(&y.to_be_bytes());
    b
}

fn gesture_scalar(subtype: u16, value: f64, phase: u64, mask: u64, x: u16, y: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(36);
    b.push(0x17);
    b.push(0);
    b.extend_from_slice(&0x0020u16.to_be_bytes());
    b.extend_from_slice(&2u16.to_be_bytes());
    b.extend_from_slice(&subtype.to_be_bytes());
    b.extend_from_slice(&value.to_be_bytes());
    b.extend_from_slice(&x.to_be_bytes());
    b.extend_from_slice(&y.to_be_bytes());
    b.extend_from_slice(&phase.to_be_bytes());
    b.extend_from_slice(&mask.to_be_bytes());
    b
}

pub fn gesture_magnify(value: f64, phase: u64, mask: u64, x: u16, y: u16) -> Vec<u8> {
    gesture_scalar(3, value, phase, mask, x, y)
}

pub fn gesture_rotate(value: f64, phase: u64, mask: u64, x: u16, y: u16) -> Vec<u8> {
    gesture_scalar(5, value, phase, mask, x, y)
}

pub fn gesture_smart_magnify(x: u16, y: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(12);
    b.push(0x17);
    b.push(0);
    b.extend_from_slice(&0x0008u16.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&4u16.to_be_bytes());
    b.extend_from_slice(&x.to_be_bytes());
    b.extend_from_slice(&y.to_be_bytes());
    b
}

pub fn gesture_quicklook(x: u16, y: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(12);
    b.push(0x17);
    b.push(0);
    b.extend_from_slice(&0x0008u16.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&7u16.to_be_bytes());
    b.extend_from_slice(&x.to_be_bytes());
    b.extend_from_slice(&y.to_be_bytes());
    b
}

pub fn gesture_swipe(
    motion: u32,
    swipe_mask: u32,
    progress: f32,
    px: f32,
    py: f32,
    vx: f32,
    vy: f32,
    vz: f32,
    flags: u32,
    flavor: u32,
    x: u16,
    y: u16,
) -> Vec<u8> {
    let mut b = Vec::with_capacity(52);
    b.push(0x17);
    b.push(0);
    b.extend_from_slice(&0x0030u16.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&6u16.to_be_bytes());
    b.extend_from_slice(&motion.to_be_bytes());
    b.extend_from_slice(&swipe_mask.to_be_bytes());
    for f in [progress, px, py, vx, vy, vz] {
        b.extend_from_slice(&f.to_be_bytes());
    }
    b.extend_from_slice(&flags.to_be_bytes());
    b.extend_from_slice(&flavor.to_be_bytes());
    b.extend_from_slice(&x.to_be_bytes());
    b.extend_from_slice(&y.to_be_bytes());
    b
}

pub fn gesture_begin_end(subtype: u16, extra: u32, x: u16, y: u16) -> Vec<u8> {
    let mut b = Vec::with_capacity(16);
    b.push(0x17);
    b.push(0);
    b.extend_from_slice(&0x000cu16.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&subtype.to_be_bytes());
    b.extend_from_slice(&extra.to_be_bytes());
    b.extend_from_slice(&x.to_be_bytes());
    b.extend_from_slice(&y.to_be_bytes());
    b
}

pub fn gesture_mouse_scroll(
    dx: i16,
    dy: i16,
    dz: i16,
    fixed: (i32, i32, i32),
    point: (i32, i32, i32),
    phase: u32,
    momentum: u32,
    x: u16,
    y: u16,
) -> Vec<u8> {
    let mut b = Vec::with_capacity(58);
    b.push(0x17);
    b.push(0);
    b.extend_from_slice(&0x0036u16.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&11u16.to_be_bytes());
    b.extend_from_slice(&dx.to_be_bytes());
    b.extend_from_slice(&dy.to_be_bytes());
    b.extend_from_slice(&dz.to_be_bytes());
    for v in [fixed.0, fixed.1, fixed.2, point.0, point.1, point.2] {
        b.extend_from_slice(&v.to_be_bytes());
    }
    b.extend_from_slice(&phase.to_be_bytes());
    b.extend_from_slice(&momentum.to_be_bytes());
    b.extend_from_slice(&x.to_be_bytes());
    b.extend_from_slice(&y.to_be_bytes());
    b
}

pub fn gesture_scroll(
    dx: f32,
    dy: f32,
    dz: f32,
    natural: u32,
    phase: u32,
    mask: u32,
    x: u16,
    y: u16,
) -> Vec<u8> {
    let mut b = Vec::with_capacity(36);
    b.push(0x17);
    b.push(0);
    b.extend_from_slice(&0x0020u16.to_be_bytes());
    b.extend_from_slice(&1u16.to_be_bytes());
    b.extend_from_slice(&8u16.to_be_bytes());
    for f in [dx, dy, dz] {
        b.extend_from_slice(&f.to_be_bytes());
    }
    b.extend_from_slice(&natural.to_be_bytes());
    b.extend_from_slice(&phase.to_be_bytes());
    b.extend_from_slice(&mask.to_be_bytes());
    b.extend_from_slice(&x.to_be_bytes());
    b.extend_from_slice(&y.to_be_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gesture_swipe_layout() {
        let b = gesture_swipe(1, 8, 1.0, 0.5, 0.5, 0.0, 0.0, 0.0, 0, 0, 1280, 800);
        assert_eq!(b.len(), 52);
        assert_eq!(b[0], 0x17);
        assert_eq!(be16(&b[2..4]), 0x30);
        assert_eq!(be16(&b[4..6]), 1);
        assert_eq!(be16(&b[6..8]), 6);
        assert_eq!(be32(&b[8..12]), 1);
        assert_eq!(be32(&b[12..16]), 8);
        assert_eq!(be16(&b[48..50]), 1280);
        assert_eq!(be16(&b[50..52]), 800);
    }

    #[test]
    fn gesture_begin_end_layout() {
        let b = gesture_begin_end(1, 0, 100, 200);
        assert_eq!(b.len(), 16);
        assert_eq!(b[0], 0x17);
        assert_eq!(be16(&b[2..4]), 12);
        assert_eq!(be16(&b[6..8]), 1);
        assert_eq!(be16(&b[12..14]), 100);
        assert_eq!(be16(&b[14..16]), 200);
    }

    #[test]
    fn key_and_pointer_layout() {
        let k = key_event(0x61, true);
        assert_eq!(k, vec![4, 1, 0, 0, 0, 0, 0, 0x61]);
        let p = pointer_event(1, 0x1234, 0x5678);
        assert_eq!(p, vec![5, 1, 0x12, 0x34, 0x56, 0x78]);
    }

    #[test]
    fn auto_framebuffer_update_layout() {
        let b = auto_framebuffer_update(2560, 1600);
        assert_eq!(b.len(), 16);
        assert_eq!(b[0], 0x09);
        assert_eq!(be16(&b[2..4]), 1);
        assert_eq!(be32(&b[4..8]), 0xffff_ffff);
        assert_eq!(be16(&b[12..14]), 2560);
        assert_eq!(be16(&b[14..16]), 1600);
    }

    #[test]
    fn record_cipher_roundtrip_is_stateful() {
        let key = [7u8; 16];
        let iv = [9u8; 16];
        let mut enc = RecordCipher::new(&key, &iv);
        let mut dec = RecordCipher::new(&key, &iv);
        let c1 = enc.encrypt(b"hello");
        assert_eq!(dec.decrypt(&c1).unwrap(), b"hello");
        let c2 = enc.encrypt(b"world, second record");
        assert_eq!(dec.decrypt(&c2).unwrap(), b"world, second record");
    }

    #[test]
    fn record_cipher_rejects_tampering() {
        let key = [1u8; 16];
        let iv = [2u8; 16];
        let mut enc = RecordCipher::new(&key, &iv);
        let mut dec = RecordCipher::new(&key, &iv);
        let mut c = enc.encrypt(b"payload");
        let last = c.len() - 1;
        c[last] ^= 0x80;
        assert!(dec.decrypt(&c).is_err());
    }
}
