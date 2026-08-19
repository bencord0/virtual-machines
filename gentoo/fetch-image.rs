#!/usr/bin/env -S cargo +nightly -Zscript
---
[package]
edition = "2021"

[dependencies]
reqwest = { version = "0.12", default-features = false, features = ["blocking", "rustls-tls"] }
sequoia-openpgp = { version = "2", default-features = false, features = ["crypto-rust", "allow-experimental-crypto", "allow-variable-time-crypto", "compression-deflate"] }
eyre = "0.6"
anyhow = "1"
indicatif = "0.17"
---

//! Fetch the latest Gentoo autobuild qcow2 image, verifying it against the
//! release-engineering key distributed in sec-keys/openpgp-keys-gentoo-release.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use eyre::{bail, eyre, Result, WrapErr};
use indicatif::{ProgressBar, ProgressStyle};

use sequoia_openpgp as openpgp;
use openpgp::cert::{Cert, CertParser};
use openpgp::parse::stream::{
    DetachedVerifierBuilder, MessageLayer, MessageStructure, VerificationHelper, VerifierBuilder,
};
use openpgp::parse::Parse;
use openpgp::policy::StandardPolicy;
use openpgp::KeyHandle;

const ARCH: &str = "amd64";
const VARIANT: &str = "di-amd64-cloudinit";

// Trust anchor: the armoured Gentoo release keys in
// sec-keys/openpgp-keys-gentoo-release.
const RELEASE_KEYS: &str = "/usr/share/openpgp-keys/gentoo-release.asc";

/// Supplies the trusted certs and enforces that at least one signature is good.
struct Helper {
    certs: Vec<Cert>,
}

impl VerificationHelper for Helper {
    fn get_certs(&mut self, _ids: &[KeyHandle]) -> openpgp::Result<Vec<Cert>> {
        Ok(self.certs.clone())
    }

    fn check(&mut self, structure: MessageStructure) -> openpgp::Result<()> {
        for layer in structure {
            if let MessageLayer::SignatureGroup { results } = layer {
                if results.iter().any(|r| r.is_ok()) {
                    return Ok(());
                }
                // Surface why every signature was rejected (e.g. an expired
                // trust key — refresh Portage's keyring with getuto/gemato).
                let reasons: Vec<String> = results
                    .iter()
                    .filter_map(|r| r.as_ref().err().map(|e| e.to_string()))
                    .collect();
                return Err(anyhow::anyhow!(
                    "no valid signature from a trusted key: {}",
                    reasons.join("; ")
                ));
            }
        }
        Err(anyhow::anyhow!("message contained no signature"))
    }
}

fn main() -> Result<()> {
    let base_url = format!("https://distfiles.gentoo.org/releases/{ARCH}/autobuilds");
    let latest = format!("latest-{VARIANT}.txt");

    // 0. Load Portage's trusted keys.
    let certs = load_certs().wrap_err("loading trust certs from gentoo")?;

    // 1. Fetch the "latest" pointer file and verify its inline PGP signature.
    let latest_url = format!("{base_url}/{latest}");
    let latest_bytes =
        fetch_bytes(&latest_url).wrap_err_with(|| format!("fetching {latest_url}"))?;
    fs::write("latest.txt", &latest_bytes).wrap_err("writing latest.txt")?;

    let verified_body = verify_inline(&latest_bytes, &certs)
        .wrap_err_with(|| format!("verifying signature on {latest}"))?;
    let verified_body =
        String::from_utf8(verified_body).wrap_err("verified latest.txt body is not UTF-8")?;

    // 2. Parse the signed body: "<relative-path>/<image>.qcow2 <size-in-bytes>".
    //    Safe to parse now that the whole file's signature has been verified.
    let (rel_path, exp_size) = parse_image_line(&verified_body)
        .wrap_err_with(|| format!("parsing image path/size from {latest}"))?;

    let image_url = format!("{base_url}/{rel_path}");
    let asc_url = format!("{image_url}.asc");
    let image_file = Path::new(&rel_path)
        .file_name()
        .ok_or_else(|| eyre!("no basename in {rel_path}"))?
        .to_str()
        .ok_or_else(|| eyre!("non-UTF-8 basename in {rel_path}"))?
        .to_owned();

    // 3. Download the image (resume-safe within a release) and its signature.
    download_resumable(&image_url, &image_file)
        .wrap_err_with(|| format!("downloading {image_url}"))?;
    let asc_file = format!("{image_file}.asc");
    let asc_bytes = fetch_bytes(&asc_url).wrap_err_with(|| format!("fetching {asc_url}"))?;
    fs::write(&asc_file, &asc_bytes).wrap_err_with(|| format!("writing {asc_file}"))?;

    // 4. Verify the downloaded size matches the signed size.
    let act_size = fs::metadata(&image_file)
        .wrap_err_with(|| format!("stat {image_file}"))?
        .len();
    if act_size != exp_size {
        bail!("size mismatch for {image_file}: got {act_size}, expected {exp_size}");
    }

    // 5. Verify the detached signature over the image bytes.
    verify_detached(&asc_bytes, &image_file, &certs)
        .wrap_err_with(|| format!("verifying detached signature on {image_file}"))?;

    // 6. Publish the verified image as image.qcow2 (a symlink; the run-*.sh
    //    scripts boot it with snapshot=on, so this pristine file is never mutated).
    symlink_force(&image_file, "image.qcow2").wrap_err("publishing image.qcow2 symlink")?;

    println!("verified {image_file} ({exp_size} bytes); image.qcow2 -> {image_file}");
    Ok(())
}

/// Parse every cert out of the armoured Gentoo release keyfile.
fn load_certs() -> Result<Vec<Cert>> {
    let certs: Vec<Cert> = CertParser::from_file(RELEASE_KEYS)
        .map_err(|e| eyre!("{e:#}"))
        .wrap_err_with(|| format!("opening {RELEASE_KEYS}"))?
        .collect::<openpgp::Result<Vec<Cert>>>()
        .map_err(|e| eyre!("{e:#}"))
        .wrap_err_with(|| format!("parsing certs from {RELEASE_KEYS}"))?;
    if certs.is_empty() {
        bail!("no certs found in {RELEASE_KEYS}; nothing to trust");
    }
    Ok(certs)
}

/// Verify an inline/cleartext-signed message, returning the verified body bytes.
fn verify_inline(signed: &[u8], certs: &[Cert]) -> Result<Vec<u8>> {
    let policy = &StandardPolicy::new();
    let helper = Helper { certs: certs.to_vec() };
    let mut verifier = VerifierBuilder::from_bytes(signed)
        .map_err(|e| eyre!("{e:#}"))?
        .with_policy(policy, None, helper)
        .map_err(|e| eyre!("{e:#}"))?;
    let mut body = Vec::new();
    verifier
        .read_to_end(&mut body)
        .map_err(|e| eyre!("{e:#}"))?;
    Ok(body)
}

/// Verify a detached signature (`sig`) over the file at `data_path`.
fn verify_detached(sig: &[u8], data_path: &str, certs: &[Cert]) -> Result<()> {
    let policy = &StandardPolicy::new();
    let helper = Helper { certs: certs.to_vec() };
    let data = File::open(data_path).wrap_err_with(|| format!("opening {data_path}"))?;
    let len = data
        .metadata()
        .wrap_err_with(|| format!("stat {data_path}"))?
        .len();
    let pb = byte_bar(len, "verifying");
    DetachedVerifierBuilder::from_bytes(sig)
        .map_err(|e| eyre!("{e:#}"))?
        .with_policy(policy, None, helper)
        .map_err(|e| eyre!("{e:#}"))?
        .verify_reader(pb.wrap_read(data))
        .map_err(|e| eyre!("{e:#}"))?;
    pb.finish_and_clear();
    Ok(())
}

/// A byte-count progress bar (rate + ETA) rendered to stderr.
fn byte_bar(len: u64, msg: &'static str) -> ProgressBar {
    let pb = ProgressBar::new(len);
    pb.set_style(
        ProgressStyle::with_template(
            "{msg} {bytes}/{total_bytes} [{wide_bar}] {bytes_per_sec} eta {eta}",
        )
        .unwrap()
        .progress_chars("=> "),
    );
    pb.set_message(msg);
    pb
}

/// GET a URL into memory, erroring on non-success status.
fn fetch_bytes(url: &str) -> Result<Vec<u8>> {
    let resp = reqwest::blocking::get(url)?.error_for_status()?;
    Ok(resp.bytes()?.to_vec())
}

/// Find the "<path>.qcow2 <size>" line in the verified body and split it.
fn parse_image_line(body: &str) -> Result<(String, u64)> {
    for line in body.lines() {
        let mut fields = line.split_whitespace();
        if let (Some(path), Some(size), None) = (fields.next(), fields.next(), fields.next()) {
            if path.ends_with(".qcow2") {
                if let Ok(size) = size.parse::<u64>() {
                    return Ok((path.to_owned(), size));
                }
            }
        }
    }
    Err(eyre!("could not find a '<path>.qcow2 <size>' line"))
}

/// Download `url` to `dest`, resuming a partial file with a Range request
/// (equivalent to `wget --continue`).
fn download_resumable(url: &str, dest: &str) -> Result<()> {
    let have = fs::metadata(dest).map(|m| m.len()).unwrap_or(0);

    let client = reqwest::blocking::Client::new();
    let mut req = client.get(url);
    if have > 0 {
        req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
    }
    let resp = req.send()?;

    // 416 = the range is unsatisfiable, i.e. we already have the whole file.
    if have > 0 && resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
        return Ok(());
    }
    let resuming = have > 0 && resp.status() == reqwest::StatusCode::PARTIAL_CONTENT;
    resp.error_for_status_ref()?;

    let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .append(resuming)
        .truncate(!resuming)
        .open(dest)
        .wrap_err_with(|| format!("opening {dest} for write"))?;

    // Total = bytes already on disk + bytes this response will deliver (on a 206
    // resume, content_length() is only the remaining range). Spinner if unknown.
    let pb = match resp.content_length() {
        Some(remaining) => byte_bar(have + remaining, "downloading"),
        None => {
            let pb = ProgressBar::new_spinner();
            pb.set_message("downloading");
            pb
        }
    };
    pb.set_position(have);

    let mut src = pb.wrap_read(resp);
    std::io::copy(&mut src, &mut file).wrap_err_with(|| format!("streaming to {dest}"))?;
    file.flush()?;
    pb.finish_and_clear();
    Ok(())
}

/// `ln -sfn target link`: create/replace `link` as a symlink to `target`.
fn symlink_force(target: &str, link: &str) -> Result<()> {
    match fs::symlink_metadata(link) {
        Ok(_) => fs::remove_file(link).wrap_err_with(|| format!("removing existing {link}"))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).wrap_err_with(|| format!("inspecting {link}")),
    }
    std::os::unix::fs::symlink(target, link)
        .wrap_err_with(|| format!("symlinking {link} -> {target}"))?;
    Ok(())
}
