//! 在线自升级（`ferry upgrade`）：检查 GitHub Releases → 下载 → sha256 校验
//! → minisign 验签 → self-replace 替换运行中的自身。
//!
//! 方案移植自 qrctrl（同作者的姊妹项目），按 CLI 形态裁剪：
//! 无 GUI 状态机/轮询；ferry 全平台发布裸二进制，无 macOS bundle 分支。
//!
//! 安全边界（纵深三层）：HTTPS 保通道 → manifest 内 sha256 保下载完整性 →
//! **minisign Ed25519 签名保真实性**（防 GitHub 账号被盗 / repo 被攻破 / 镜像
//! 篡改——攻击者拿不到私钥就产不出能通过验签的 manifest）。公钥编译期内嵌
//! （`assets/update-pubkey.txt`），私钥只在 `.sign/`（gitignore）与 GitHub Secrets。
//!
//! **FERRY_MIRROR 的安全性来源正是签名**：受限网络的镜像前缀（如
//! `https://ghproxy.net/`）本身不可信没关系——镜像敢篡改 manifest 或资产，
//! 验签必失败。传输不可信、签名可信。
//!
//! manifest 是信封结构 `{payload, signature}`：签名对 payload 字符串的原始
//! 字节，客户端不重新序列化，验签字节与服务端签名字节完全一致。
//!
//! 更新源硬编码（`MANIFEST_URL`）：一旦可配置，签名体系就有被指向恶意服务
//! 器的社会工程入口；镜像需求由 FERRY_MIRROR 前缀满足（签名兜底）。
//!
//! 本模块不引用 crate:: 之外的工程内路径——`examples/verify_manifest.rs` 通过
//! `#[path]` 直接引入，走的就是客户端同一条验证代码，不是复制品。

use std::io::Read;
use std::path::Path;
use std::time::Duration;

use sha2::{Digest, Sha256};

/// manifest 挂在「最新非 draft 非 prerelease 的 release」下。走 GitHub 的
/// `releases/latest/download/<asset>` 稳定重定向端点而不是 REST API——后者
/// 未认证限速 60 次/小时/源 IP，前者走 release CDN 不占额度。draft release
/// 不会被 `latest` 命中：发布后必须 publish，否则客户端静默查不到更新。
const MANIFEST_URL: &str =
    "https://github.com/wuyan19/ferry/releases/latest/download/update-manifest.json";
/// 下载硬上限，防 manifest 被篡改后 size 巨大撑爆磁盘。ferry 产物 < 10 MB。
const MAX_DOWNLOAD: u64 = 100 * 1024 * 1024;
const CHECK_TIMEOUT: Duration = Duration::from_secs(15);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// GitHub 强制要求 User-Agent，否则 403。
const USER_AGENT: &str = concat!("ferry/", env!("CARGO_PKG_VERSION"));
/// 当前平台的 manifest key = 编译目标三元组（build.rs 注入），与 release.yml
/// 的资产命名 `ferry-<tag>-<target>` 一一对应。
const PLATFORM_KEY: &str = env!("FERRY_TARGET");
/// 更新签名公钥（minisign 公钥文件全文）。密钥轮换：生成新密钥对后在数组
/// 追加新公钥文件，发一个过渡版本让存量客户端信任新钥，之后移除旧公钥。
const UPDATE_PUBKEYS: &[&str] = &[include_str!("../assets/update-pubkey.txt")];

// ===== 数据模型 =====

/// manifest 的外层信封。`payload` 是内层 UpdateManifest 的 JSON **字符串**，
/// `signature` 是对 payload 字节的 minisign 签名（.sig 全文）。
///
/// 为什么套一层字符串而不是直接签外层 JSON：验签必须针对**确定字节**。签名
/// 对象若是「反序列化后重新序列化的 JSON」，两端的 key 顺序/空白/转义差异
/// 都会让验签失败；把 payload 当不透明字符串签名，客户端验证的就是服务端
/// 签名的原文，验完再 parse——字节确定性与解析自由度兼得。
#[derive(Debug, serde::Deserialize)]
struct SignedManifest {
    payload: String,
    signature: String,
}

/// CI 生成的更新清单（`SignedManifest.payload` 的内容）。字段与 release.yml
/// manifest job 的生成脚本约定，改名必须两边同步。
#[derive(Debug, serde::Deserialize)]
pub struct UpdateManifest {
    pub version: String,
    /// 更新日志页（release 页本身）。可为空。
    #[serde(default)]
    pub notes_url: String,
    /// 平台 key（编译目标三元组）→ 资产。
    pub platforms: std::collections::BTreeMap<String, PlatformAsset>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct PlatformAsset {
    pub url: String,
    pub size: u64,
    /// sha256 hex，快速失败层：下载流式算，不匹配立即中止。
    pub sha256: String,
    /// minisign 签名（.sig 全文），对资产文件字节。真实性别。
    #[serde(default)]
    pub signature: String,
}

/// 一次检查的结果。
#[derive(Debug)]
pub enum UpdateOutcome {
    UpToDate,
    Available {
        version: String,
        notes_url: String,
        asset: PlatformAsset,
    },
}

#[derive(Debug)]
pub enum UpdateError {
    Network(String),
    Manifest(String),
    Io(String),
    Checksum,
    /// 验签失败：更新内容不可信，已中止并删除半成品。绝不降级为跳过验签。
    InvalidSignature(String),
    UnsupportedPlatform,
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Network(e) => write!(f, "network error: {e}"),
            Self::Manifest(e) => write!(f, "invalid update manifest: {e}"),
            Self::Io(e) => write!(f, "local file operation failed: {e}"),
            Self::Checksum => write!(f, "sha256 mismatch, download corrupted; retry"),
            Self::InvalidSignature(d) => {
                write!(f, "signature verification failed ({d}) — update may be tampered, aborted")
            }
            Self::UnsupportedPlatform => {
                write!(f, "no update channel for this platform ({PLATFORM_KEY}); install manually")
            }
        }
    }
}

impl std::error::Error for UpdateError {}

// ===== 纯逻辑（单测覆盖）=====

/// latest 是否比 current 新。容忍 `v` 前缀（GitHub tag 惯例）。
/// 任一侧解析失败按「不新」处理——宁可漏一次更新，也不能把坏版本号当新版。
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

fn parse_version(v: &str) -> Option<semver::Version> {
    semver::Version::parse(v.trim().trim_start_matches('v')).ok()
}

pub fn parse_manifest(body: &str) -> Result<UpdateManifest, UpdateError> {
    let m: UpdateManifest = serde_json::from_str(body)
        .map_err(|e| UpdateError::Manifest(format!("JSON parse failed: {e}")))?;
    if parse_version(&m.version).is_none() {
        return Err(UpdateError::Manifest(format!("invalid version field: {:?}", m.version)));
    }
    Ok(m)
}

fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ===== 验签（纯逻辑，单测覆盖）=====

/// minisign 公钥文件（comment 行 + base64 行）→ PublicKey。
/// 取最后一个非空行为 base64，兼容标准 minisign CLI 产出的文件。
fn public_key_from_file_content(content: &str) -> Result<minisign_verify::PublicKey, UpdateError> {
    let b64 = content
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .ok_or_else(|| UpdateError::InvalidSignature("public key file is empty".into()))?;
    minisign_verify::PublicKey::from_base64(b64.trim())
        .map_err(|e| UpdateError::InvalidSignature(format!("public key parse failed: {e}")))
}

/// 用给定公钥列表验签（任一通过即可——轮换过渡期新旧并存）。
/// 测试注入临时密钥，生产走 [`UPDATE_PUBKEYS`]。
fn verify_with_keys(data: &[u8], sig_text: &str, keys: &[&str]) -> Result<(), UpdateError> {
    let sig_text = sig_text.trim();
    if sig_text.is_empty() {
        return Err(UpdateError::InvalidSignature("missing signature field".into()));
    }
    let signature = minisign_verify::Signature::decode(sig_text)
        .map_err(|e| UpdateError::InvalidSignature(format!("malformed signature: {e}")))?;
    let public_keys: Vec<_> = keys
        .iter()
        .map(|k| public_key_from_file_content(k))
        .collect::<Result<Vec<_>, _>>()?;
    for pk in &public_keys {
        // allow_legacy：兼容系统 minisign CLI 手动补签的非 prehash 签名。
        if pk.verify(data, &signature, true).is_ok() {
            return Ok(());
        }
    }
    Err(UpdateError::InvalidSignature("no embedded public key verifies".into()))
}

fn verify_signature(data: &[u8], sig_text: &str) -> Result<(), UpdateError> {
    verify_with_keys(data, sig_text, UPDATE_PUBKEYS)
}

// ===== IO 动作（同步阻塞；CLI 一次性调用，无需 spawn_blocking）=====

fn mirror() -> String {
    std::env::var("FERRY_MIRROR").unwrap_or_default()
}

fn http_agent(timeout: Duration) -> ureq::Agent {
    // 注入纯 Rust CryptoProvider（ureq 关掉了默认 ring）：真实性由 minisign
    // 签名端到端兜底，TLS 栈只承担通道加密，纯 Rust 实现的性能足够拉 manifest
    // 和 ~3.5MB 的资产。
    let tls = ureq::tls::TlsConfig::builder()
        // ureq 明示的非稳定 API：rustls 未到 1.x，跨小版本可能变动（编译期即暴露）
        .unversioned_rustls_crypto_provider(std::sync::Arc::new(rustls_rustcrypto::provider()))
        .build();
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .tls_config(tls)
        .build()
        .into()
}

/// 404 有专门语义（尚无已发布 release / 忘了 publish），单独映射。
fn map_http_error(e: ureq::Error) -> UpdateError {
    match &e {
        ureq::Error::StatusCode(s) if *s == 404 => UpdateError::Manifest(
            "update manifest not found (no published release yet, or release not published)".into(),
        ),
        _ => UpdateError::Network(format!("{e}")),
    }
}

fn io_err(ctx: &str) -> impl Fn(std::io::Error) -> UpdateError + '_ {
    move |e| UpdateError::Io(format!("{ctx}: {e}"))
}

/// 信封解析 + 验签 + 内层解析（check_latest 与 verify_manifest example 共用，
/// 也是发布后 smoke 自检走到的同一条路径）。
pub fn parse_signed_manifest(body: &str) -> Result<UpdateManifest, UpdateError> {
    let signed: SignedManifest = serde_json::from_str(body).map_err(|e| {
        UpdateError::Manifest(format!("envelope parse failed (expect {{payload, signature}}): {e}"))
    })?;
    // 验签先于一切内容解析——payload 在验签通过前是不可信输入。
    verify_signature(signed.payload.as_bytes(), &signed.signature)?;
    parse_manifest(&signed.payload)
}

/// 拉取 manifest（信封验签 → parse 内层）并与当前版本比较。
pub fn check_latest(current: &str) -> Result<UpdateOutcome, UpdateError> {
    let url = format!("{}{MANIFEST_URL}", mirror());
    let body = http_agent(CHECK_TIMEOUT)
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(map_http_error)?
        .body_mut()
        .read_to_string()
        .map_err(|e| UpdateError::Network(format!("reading manifest failed: {e}")))?;

    let manifest = parse_signed_manifest(&body)?;
    let asset = manifest
        .platforms
        .get(PLATFORM_KEY)
        .cloned()
        .ok_or(UpdateError::UnsupportedPlatform)?;

    if is_newer(&manifest.version, current) {
        Ok(UpdateOutcome::Available {
            version: manifest.version,
            notes_url: manifest.notes_url,
            asset,
        })
    } else {
        Ok(UpdateOutcome::UpToDate)
    }
}

/// 流式下载资产到 `dest`，边下边算 sha256，再对落盘字节验签。
/// 任何失败删除半成品，不留垃圾。
pub fn download_and_verify(asset: &PlatformAsset, dest: &Path) -> Result<(), UpdateError> {
    if asset.size > MAX_DOWNLOAD {
        return Err(UpdateError::Manifest(format!(
            "declared asset size {} exceeds safety limit",
            asset.size
        )));
    }
    let url = format!("{}{}", mirror(), asset.url);
    let mut resp = http_agent(DOWNLOAD_TIMEOUT)
        .get(&url)
        .header("User-Agent", USER_AGENT)
        .call()
        .map_err(map_http_error)?;

    let file = std::fs::File::create(dest).map_err(io_err("create temp file"))?;
    let mut writer = std::io::BufWriter::new(file);
    let mut hasher = Sha256::new();
    let mut reader = resp.body_mut().as_reader();
    let mut buf = vec![0u8; 64 * 1024];
    let mut done: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| UpdateError::Network(format!("download interrupted: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        std::io::Write::write_all(&mut writer, &buf[..n]).map_err(io_err("write temp file"))?;
        done += n as u64;
        if done > MAX_DOWNLOAD {
            // 声明被伪造小、实际超大：立即止损。
            let _ = std::fs::remove_file(dest);
            return Err(UpdateError::Manifest(format!(
                "downloaded content exceeds safety limit {MAX_DOWNLOAD} bytes"
            )));
        }
    }
    std::io::Write::flush(&mut writer).map_err(io_err("flush temp file"))?;

    let actual = to_hex(&hasher.finalize());
    if !actual.eq_ignore_ascii_case(asset.sha256.trim()) {
        let _ = std::fs::remove_file(dest);
        return Err(UpdateError::Checksum);
    }

    // 真实性别：对资产字节验签。验签失败绝不继续安装。
    let data = std::fs::read(dest).map_err(io_err("read back download"))?;
    if let Err(e) = verify_signature(&data, &asset.signature) {
        let _ = std::fs::remove_file(dest);
        return Err(e);
    }
    Ok(())
}

/// 把已下载并通过校验的资产替换为当前运行中的自身。
/// 全平台裸二进制：Unix 靠 inode 语义，Windows 由 self-replace 做 rename 舞步。
pub fn install_update(downloaded: &Path) -> Result<(), UpdateError> {
    #[cfg(unix)]
    set_executable(downloaded)?;
    self_replace::self_replace(downloaded)
        .map_err(|e| UpdateError::Io(format!("self-replace failed: {e}")))
}

/// 下载落盘的文件默认无执行位，Unix 上直接换过去会让新进程 spawn 失败。
#[cfg(unix)]
fn set_executable(p: &Path) -> Result<(), UpdateError> {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(p).map_err(io_err("stat temp file"))?.permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(p, perm).map_err(io_err("chmod temp file"))
}

/// `ferry upgrade` 的完整流程。返回给用户看的总结行。
pub fn upgrade(check_only: bool) -> Result<String, UpdateError> {
    let current = env!("CARGO_PKG_VERSION");
    match check_latest(current)? {
        UpdateOutcome::UpToDate => Ok(format!("ferry {current} is up to date")),
        UpdateOutcome::Available { version, notes_url, asset } => {
            if check_only {
                return Ok(format!(
                    "update available: {current} -> {version} (run `ferry upgrade`){}",
                    if notes_url.is_empty() {
                        String::new()
                    } else {
                        format!("\nnotes: {notes_url}")
                    }
                ));
            }
            let mut tmp = std::env::temp_dir();
            tmp.push(format!("ferry-upgrade-{}.bin", std::process::id()));
            download_and_verify(&asset, &tmp)?;
            install_update(&tmp)?;
            let _ = std::fs::remove_file(&tmp);
            Ok(format!("upgraded: {current} -> {version} (run `ferry --version` to confirm)"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha256_hex(data: &[u8]) -> String {
        let mut h = Sha256::new();
        h.update(data);
        to_hex(&h.finalize())
    }

    #[test]
    fn is_newer_handles_versions_and_prefix() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.0"));
        assert!(is_newer("1.0.0", "v0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"), "相等不算新");
        assert!(!is_newer("0.1.0", "0.2.0"), "更旧不算新");
        assert!(!is_newer("junk", "0.1.0"), "坏版本号按不新处理");
        // semver 语义：0.10.0 > 0.9.0（按数字段比，不是字符串前缀）
        assert!(is_newer("0.10.0", "0.9.9"));
    }

    #[test]
    fn parse_manifest_accepts_full_shape() {
        let body = r#"{
            "version": "1.2.3",
            "notes_url": "https://example.com/notes",
            "platforms": {
                "x86_64-unknown-linux-musl": {"url": "https://e.com/a", "size": 100, "sha256": "aa"}
            }
        }"#;
        let m = parse_manifest(body).expect("合法 manifest");
        assert_eq!(m.version, "1.2.3");
        assert_eq!(m.notes_url, "https://example.com/notes");
        assert_eq!(m.platforms["x86_64-unknown-linux-musl"].size, 100);
    }

    #[test]
    fn parse_manifest_defaults_notes_url() {
        let m = parse_manifest(r#"{"version":"1.0.0","platforms":{}}"#).expect("notes_url 缺省应容忍");
        assert_eq!(m.notes_url, "");
    }

    #[test]
    fn parse_manifest_rejects_bad_input() {
        assert!(parse_manifest("not json").is_err());
        assert!(parse_manifest("{}").is_err(), "缺 version / platforms");
        assert!(parse_manifest(r#"{"version":"xyz","platforms":{}}"#).is_err());
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff]), "000fff");
    }

    // ===== 签名验签（生产代码 verify_with_keys + 测试密钥对）=====
    // 生成/签名用 dev-dep 的 minisign，验证端走生产代码路径——
    // 测的就是客户端真实会执行的逻辑。

    fn test_keypair() -> (String, minisign::SecretKey) {
        let minisign::KeyPair { pk, sk } = minisign::KeyPair::generate_unencrypted_keypair().unwrap();
        (pk.to_box().unwrap().to_string(), sk)
    }

    fn test_sign(sk: &minisign::SecretKey, data: &[u8]) -> String {
        String::from(minisign::sign(None, sk, data, None, None).unwrap())
    }

    #[test]
    fn signature_roundtrip_accepts_valid() {
        let (pk, sk) = test_keypair();
        let data = br#"{"version":"1.0.0"}"#;
        let sig = test_sign(&sk, data);
        assert!(verify_with_keys(data, &sig, &[&pk]).is_ok());
    }

    #[test]
    fn signature_rejects_tampered_payload() {
        let (pk, sk) = test_keypair();
        let sig = test_sign(&sk, br#"{"version":"1.0.0"}"#);
        // 篡改后的 payload：同样的签名必须验不过
        assert!(verify_with_keys(br#"{"version":"9.9.9"}"#, &sig, &[&pk]).is_err());
    }

    #[test]
    fn signature_rejects_wrong_key_and_empty() {
        let (_pk1, sk) = test_keypair();
        let (pk2, _sk2) = test_keypair();
        let data = b"payload";
        let sig = test_sign(&sk, data);
        assert!(verify_with_keys(data, &sig, &[&pk2]).is_err(), "另一把公钥必须验不过");
        assert!(verify_with_keys(data, "", &[&pk2]).is_err(), "空签名必须失败");
    }

    #[test]
    fn envelope_verifies_payload_string_bytes() {
        // 信封语义：签名对 payload 字符串的原始字节。构造真实信封验证整条路径。
        let (pk, sk) = test_keypair();
        let payload = r#"{"version":"0.2.0","notes_url":"","platforms":{}}"#;
        let sig = test_sign(&sk, payload.as_bytes());
        let envelope = serde_json::json!({"payload": payload, "signature": sig});
        let body = envelope.to_string();
        // 用测试公钥替换内嵌公钥走 parse_signed_manifest 的等价逻辑
        let signed: SignedManifest = serde_json::from_str(&body).unwrap();
        assert!(verify_with_keys(signed.payload.as_bytes(), &signed.signature, &[&pk]).is_ok());
        // 重新序列化的 envelope 字节序可能变化，但 payload 字符串字节不变——
        // 这正是「签字符串不签外层 JSON」的意义
        let m = parse_manifest(&signed.payload).unwrap();
        assert_eq!(m.version, "0.2.0");
    }

    #[test]
    fn embedded_pubkey_parses() {
        // 内嵌公钥文件必须可解析（格式坏 = 所有更新静默失败，编译期就该发现）
        for k in UPDATE_PUBKEYS {
            public_key_from_file_content(k).expect("内嵌公钥格式非法");
        }
    }
}
