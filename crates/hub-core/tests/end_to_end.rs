//! 端到端集成测试:read → write 全链路(TC-E2E-01/02)。
//!
//! 归一化规则见:
//! uuid v4 → `<UUID>`;RFC3339 时间戳(两种格式)→ `<TS>`;`"cwd":"<任意>"` → `"cwd":"<CWD>"`;
//! 路径前缀 `^\d{4}/\d{2}/\d{2}/` → `<DATE>/`。固定 IdGen 消除其余不确定性。

use std::fs;
use std::path::PathBuf;

use chrono::DateTime;
use hub_core::{read_session, write_session_with, IdGen};
use serde_json::Value;

const FIXED_UUID: &str = "00000000-0000-4000-8000-000000000001";
const FIXED_TS_FILE: &str = "2026-09-11T00-00-00";
const FIXED_TS_COLON: &str = "2026-09-11T00:00:00.000Z";
const FIXED_DATE: &str = "2026/09/11";

struct FixedIdGen;

impl IdGen for FixedIdGen {
    fn now_rfc3339(&self) -> String {
        FIXED_TS_FILE.to_string()
    }
    fn now_rfc3339_colon(&self) -> String {
        FIXED_TS_COLON.to_string()
    }
    fn now_date_path(&self) -> String {
        FIXED_DATE.to_string()
    }
    fn uuid_v4(&self) -> String {
        FIXED_UUID.to_string()
    }
}

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

// ---------- TC-E2E-01:黄金文件对比 + 源只读 ----------

#[test]
fn tc_e2e_01_golden_compare_and_source_untouched() {
    let source = fixture("minimal.jsonl");
    let source_bytes_before = fs::read(&source).unwrap();
    let source_sha_before = hex(&sha256(&source_bytes_before));

    // read → write
    let ir = read_session(&source).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let out = write_session_with(&ir, tmp.path(), &FixedIdGen).unwrap();

    // 产物路径形态:<root>/<DATE>/rollout-<ts>-<uuid>.jsonl
    let rel = out
        .file_path
        .strip_prefix(tmp.path())
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert_eq!(
        normalize_path_prefix(&rel),
        format!("<DATE>/rollout-{FIXED_TS_FILE}-{FIXED_UUID}.jsonl")
    );

    // 源文件迁移前后 SHA-256 不变
    let source_bytes_after = fs::read(&source).unwrap();
    let source_sha_after = hex(&sha256(&source_bytes_after));
    assert_eq!(
        source_sha_before, source_sha_after,
        "源文件 SHA-256 迁移前后必须不变"
    );

    // 归一化后与 golden 逐行语义对比
    let artifact = fs::read_to_string(&out.file_path).unwrap();
    let golden = fs::read_to_string(fixture("golden-minimal.rollout.jsonl")).unwrap();
    let artifact_lines = normalized_lines(&artifact);
    let golden_lines = normalized_lines(&golden);
    assert_eq!(
        artifact_lines.len(),
        golden_lines.len(),
        "行数一致\n产物:\n{artifact}\ngolden:\n{golden}"
    );
    for (i, (a, g)) in artifact_lines.iter().zip(&golden_lines).enumerate() {
        assert_eq!(
            a,
            g,
            "第 {} 行归一化后不一致\n产物: {a}\ngolden: {g}",
            i + 1
        );
    }
}

// ---------- TC-E2E-02:rich 全链路 ----------

#[test]
fn tc_e2e_02_rich_pipeline_succeeds_with_warnings() {
    let source = fixture("rich.jsonl");
    let ir = read_session(&source).unwrap();
    assert!(ir.parse_warnings > 0, "rich fixture 必含坏行");

    let tmp = tempfile::tempdir().unwrap();
    let out = write_session_with(&ir, tmp.path(), &FixedIdGen).unwrap();
    assert!(out.file_path.is_file());

    // 全行合法 JSON 且 timestamp 均为 RFC3339
    let artifact = fs::read_to_string(&out.file_path).unwrap();
    for line in artifact.lines() {
        let value: Value = serde_json::from_str(line).expect("每行合法 JSON");
        let ts = value["timestamp"].as_str().expect("timestamp 存在");
        assert!(DateTime::parse_from_rfc3339(ts).is_ok(), "RFC3339: {ts}");
    }

    // 文本化工具内容确实进入产物
    assert!(artifact.contains("[调用工具 Bash]"));
    assert!(artifact.contains("[工具结果 Bash isError=false]"));
    assert!(artifact.contains("> 内部推理:"));
    // sidechain 分支内容绝不出现
    assert!(!artifact.contains("sidechain 分支消息"));
}

// ---------- 归一化 ----------

fn normalized_lines(content: &str) -> Vec<Value> {
    content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let mut value: Value =
                serde_json::from_str(l).unwrap_or_else(|e| panic!("非法 JSON 行 {l}: {e}"));
            normalize_value(&mut value);
            value
        })
        .collect()
}

fn normalize_value(value: &mut Value) {
    match value {
        Value::String(s) => {
            if is_uuid_v4(s) {
                *s = "<UUID>".to_string();
            } else if DateTime::parse_from_rfc3339(s).is_ok() {
                *s = "<TS>".to_string();
            }
        }
        Value::Array(items) => items.iter_mut().for_each(normalize_value),
        Value::Object(map) => {
            for (key, v) in map {
                if key == "cwd" {
                    if let Value::String(s) = v {
                        *s = "<CWD>".to_string();
                    }
                } else {
                    normalize_value(v);
                }
            }
        }
        _ => {}
    }
}

fn is_uuid_v4(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
        && b[14] == b'4'
        && matches!(b[19], b'8' | b'9' | b'a' | b'b')
}

/// 路径前缀 `^\d{4}/\d{2}/\d{2}/` → `<DATE>/`
fn normalize_path_prefix(path: &str) -> String {
    let b = path.as_bytes();
    let shape = |i: usize, n: usize| b[i..i + n].iter().all(|c| c.is_ascii_digit());
    if b.len() > 11
        && shape(0, 4)
        && b[4] == b'/'
        && shape(5, 2)
        && b[7] == b'/'
        && shape(8, 2)
        && b[10] == b'/'
    {
        format!("<DATE>/{}", &path[11..])
    } else {
        path.to_string()
    }
}

// ---------- SHA-256(白名单外不加 sha2 crate,测试内手写标准实现) ----------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn sha256(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let mut msg = data.to_vec();
    let bit_len = (data.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                chunk[4 * i],
                chunk[4 * i + 1],
                chunk[4 * i + 2],
                chunk[4 * i + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let (mut a, mut b, mut c, mut d) = (h[0], h[1], h[2], h[3]);
        let (mut e, mut f, mut g, mut hh) = (h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut out = [0u8; 32];
    for (i, word) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// 校验手写 SHA-256 与公开已知向量一致(防实现自错自证)。
#[test]
fn sha256_known_vectors() {
    assert_eq!(
        hex(&sha256(b"abc")),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    assert_eq!(
        hex(&sha256(b"")),
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    // >64 字节跨块向量
    assert_eq!(
        hex(&sha256(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
    );
}

/// 归一化规则单元自检。
#[test]
fn normalization_helpers() {
    assert!(is_uuid_v4(FIXED_UUID));
    assert!(!is_uuid_v4("00000000-0000-3000-8000-000000000001")); // 非 v4
    assert!(!is_uuid_v4("not-a-uuid"));
    assert_eq!(
        normalize_path_prefix("2026/09/11/x.jsonl"),
        "<DATE>/x.jsonl"
    );
    assert_eq!(
        normalize_path_prefix("foo/2026/09/11.jsonl"),
        "foo/2026/09/11.jsonl"
    );

    let mut value: Value =
        serde_json::from_str(r#"{"cwd":"/any/path","id":"00000000-0000-4000-8000-000000000001","nested":{"ts":"2026-09-11T00:00:00.000Z"}}"#)
            .unwrap();
    normalize_value(&mut value);
    assert_eq!(value["cwd"], "<CWD>");
    assert_eq!(value["id"], "<UUID>");
    assert_eq!(value["nested"]["ts"], "<TS>");
}
