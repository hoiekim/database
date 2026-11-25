//! Who may connect: one password per database, and the administrator's.
//!
//! Each database's password is kept as a salted SHA-256 in the file
//! `ACCESS` of the data directory, one line per database:
//!
//! ```text
//! <name> <salt, hex> <hash, hex>
//! ```
//!
//! The passwords are machine-made secrets from the clients' environment
//! files, long and random, so a fast hash is enough: there is nothing to
//! guess. The administrator's password is not stored at all: it comes
//! from the server's own environment at every start.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74,
    0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d,
    0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e,
    0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5,
    0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// SHA-256, as FIPS 180-4 defines it.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&(data.len() as u64 * 8).to_be_bytes());
    for block in message.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (state, value) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *state = state.wrapping_add(value);
        }
    }
    let mut out = [0u8; 32];
    for (chunk, word) in out.chunks_mut(4).zip(h) {
        chunk.copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Whether two byte strings are equal, in time that depends only on
/// their lengths.
pub fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

fn hash(salt: &[u8], password: &str) -> [u8; 32] {
    let mut input = salt.to_vec();
    input.extend_from_slice(password.as_bytes());
    sha256(&input)
}

fn random_salt() -> [u8; 16] {
    let mut salt = [0u8; 16];
    let filled = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut salt)).is_ok();
    if !filled {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        salt.copy_from_slice(&sha256(&nanos.to_le_bytes())[..16]);
    }
    salt
}

/// What a password is known by without being kept: a fresh salt and the
/// hash of the two, 48 bytes. This is what a role's password is stored
/// as, in its database.
pub fn secret(password: &str) -> Vec<u8> {
    let salt = random_salt();
    [salt.as_slice(), hash(&salt, password).as_slice()].concat()
}

/// Whether `password` is the one a secret was made from. The same work
/// whatever the secret is, a missing one included, so that the answer's
/// timing does not tell which names exist.
pub fn matches(secret: Option<&[u8]>, password: &str) -> bool {
    match secret {
        Some(secret) if secret.len() == 48 => same(&hash(&secret[..16], password), &secret[16..]),
        _ => {
            let _ = hash(&[0; 16], password);
            false
        }
    }
}

/// The passwords' salts and hashes, by database.
pub struct Access {
    path: PathBuf,
    entries: BTreeMap<String, (Vec<u8>, Vec<u8>)>,
}

impl Access {
    pub fn load(dir: &Path) -> std::io::Result<Access> {
        let path = dir.join("ACCESS");
        let mut entries = BTreeMap::new();
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                for line in text.lines() {
                    let mut parts = line.split(' ');
                    if let (Some(name), Some(salt), Some(hash), None) = (parts.next(), parts.next().and_then(unhex), parts.next().and_then(unhex), parts.next()) {
                        entries.insert(name.to_string(), (salt, hash));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        Ok(Access { path, entries })
    }

    /// Writes the file whole, beside the old one, then swaps them, so a
    /// crash leaves one or the other.
    fn save(&self) -> std::io::Result<()> {
        let temp = self.path.with_extension("new");
        let mut file = std::fs::File::create(&temp)?;
        for (name, (salt, hash)) in &self.entries {
            writeln!(file, "{name} {} {}", hex(salt), hex(hash))?;
        }
        file.sync_all()?;
        std::fs::rename(&temp, &self.path)?;
        if let Some(dir) = self.path.parent() {
            std::fs::File::open(dir)?.sync_all()?;
        }
        Ok(())
    }

    pub fn check(&self, database: &str, password: &str) -> bool {
        match self.entries.get(database) {
            Some((salt, stored)) => same(&hash(salt, password), stored),
            // The same work for a database that does not exist, so the
            // answer's timing does not tell which names do.
            None => {
                let _ = hash(&[0; 16], password);
                false
            }
        }
    }

    pub fn set(&mut self, database: &str, password: &str) -> std::io::Result<()> {
        let salt = random_salt();
        self.entries.insert(database.to_string(), (salt.to_vec(), hash(&salt, password).to_vec()));
        self.save()
    }

    pub fn remove(&mut self, database: &str) -> std::io::Result<()> {
        if self.entries.remove(database).is_some() {
            self.save()?;
        }
        Ok(())
    }

    /// The salt and hash as one note, to travel in a dump.
    pub fn note(&self, database: &str) -> Vec<u8> {
        match self.entries.get(database) {
            Some((salt, hash)) => [salt.as_slice(), hash.as_slice()].concat(),
            None => Vec::new(),
        }
    }

    /// Takes a salt and hash from a dump.
    pub fn set_note(&mut self, database: &str, note: &[u8]) -> std::io::Result<()> {
        if note.len() == 48 {
            self.entries.insert(database.to_string(), (note[..16].to_vec(), note[16..].to_vec()));
            self.save()?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_the_standard_vectors() {
        assert_eq!(hex(&sha256(b"")), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(hex(&sha256(b"abc")), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(hex(&sha256(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq")), "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1");
        assert_eq!(hex(&sha256(&[b'a'; 1000])), "41edece42d63e8d9bf515a9ba6932e1c20cbc9f5a5d134645adb5db1b9737ea3");
    }

    #[test]
    fn a_secret_knows_its_password_and_no_other() {
        let made = secret("s3cret");
        assert!(matches(Some(&made), "s3cret"));
        assert!(!matches(Some(&made), "s3cret ") && !matches(Some(&made), ""));
        assert!(!matches(None, "s3cret") && !matches(Some(&made[..47]), "s3cret") && !matches(Some(&[]), ""));
        assert_ne!(secret("s3cret"), made, "each has a salt of its own");
    }

    #[test]
    fn passwords_are_checked_and_kept_across_a_restart() {
        let dir = std::env::temp_dir().join(format!("database-access-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut access = Access::load(&dir).unwrap();
        access.set("shop", "s3cret").unwrap();
        access.set("blog", "other").unwrap();
        assert!(access.check("shop", "s3cret"));
        assert!(!access.check("shop", "other"));
        assert!(!access.check("nobody", "s3cret"));

        let mut again = Access::load(&dir).unwrap();
        assert!(again.check("shop", "s3cret") && again.check("blog", "other"));
        let note = again.note("shop");
        again.remove("shop").unwrap();
        assert!(!again.check("shop", "s3cret"));
        again.set_note("shop", &note).unwrap();
        assert!(Access::load(&dir).unwrap().check("shop", "s3cret"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
