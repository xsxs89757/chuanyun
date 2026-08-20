//! 隧道命名与子域名规则。
//!
//! 规则集中在服务端裁决（客户端不自行拼装子域名），但校验逻辑放在协议层，
//! 好让客户端在发请求前就能给出人话提示，不必等一个来回。

use crate::error::code;

/// 保留字：这些名字要么被基础设施占用，要么容易被误认为官方入口。
///
/// 只对**用户名**生效，不限制隧道名。原因是隧道主机名的形态是 `{user}-{name}`，
/// 必然含连字符，而保留字都不含连字符——两者不可能相撞。所以 `api`、`admin` 这类
/// 词完全可以当隧道名用（`zhangsan-api` 既好读也不冒充 `api.t.example.com`），
/// 但不能当用户名，否则会出现 `admin-xxx` 这种看着像官方的地址。
pub const RESERVED: &[&str] = &[
    "admin",
    "api",
    "www",
    "mail",
    "ftp",
    "ns",
    "ns1",
    "ns2",
    "smtp",
    "pop",
    "imap",
    "webmail",
    "cdn",
    "static",
    "assets",
    "img",
    "test",
    "stage",
    "staging",
    "prod",
    "production",
    "dev",
    "console",
    "dashboard",
    "status",
    "health",
    "download",
    "downloads",
    "docs",
    "help",
    "support",
    "login",
    "auth",
    "sso",
    "root",
    "system",
    "chuanyun",
    "cy",
];

/// 单段名称长度上限：`{user}-{name}` 合起来不能触碰 DNS 标签的 63 字节限制。
pub const MAX_LEN: usize = 24;

/// 校验名称的形态：DNS 标签的子集——小写字母、数字、连字符，且不以连字符开头或结尾。
///
/// 隧道名用这个（不查保留字，理由见 [`RESERVED`]）；用户名用 [`validate_user`]。
/// 返回 `Err(错误码)`，调用方用 [`crate::error::human`] 转成文案。
pub fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > MAX_LEN {
        return Err(code::NAME_INVALID);
    }
    if name.starts_with('-') || name.ends_with('-') {
        return Err(code::NAME_INVALID);
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(code::NAME_INVALID);
    }
    Ok(())
}

/// 校验用户名：形态合法，且不是保留字（用户名会成为所有隧道地址的前缀）。
pub fn validate_user(user: &str) -> Result<(), &'static str> {
    validate_name(user)?;
    if RESERVED.contains(&user) {
        return Err(code::NAME_RESERVED);
    }
    Ok(())
}

/// 按约定拼装隧道的完整主机名：`{user}-{name}.{suffix}`。
///
/// 两段应各自先校验过；这里只做拼装，不重复校验。
pub fn host_for(user: &str, name: &str, domain_suffix: &str) -> String {
    format!("{user}-{name}.{domain_suffix}")
}

/// 从 Host 头里剥掉端口，拿到纯主机名。
pub fn host_without_port(host: &str) -> &str {
    match host.rfind(':') {
        // IPv6 字面量形如 [::1]:8080，括号内的冒号不是端口分隔符
        Some(i) if !host[i..].contains(']') => &host[..i],
        _ => host,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_names() {
        assert!(validate_name("api").is_ok());
        assert!(validate_name("wx-callback").is_ok());
        assert!(validate_name("p5678").is_ok());
    }

    #[test]
    fn rejects_bad_shapes() {
        assert_eq!(validate_name(""), Err(code::NAME_INVALID));
        assert_eq!(validate_name("-lead"), Err(code::NAME_INVALID));
        assert_eq!(validate_name("trail-"), Err(code::NAME_INVALID));
        assert_eq!(validate_name("Upper"), Err(code::NAME_INVALID));
        assert_eq!(validate_name("under_score"), Err(code::NAME_INVALID));
        assert_eq!(
            validate_name(&"x".repeat(MAX_LEN + 1)),
            Err(code::NAME_INVALID)
        );
    }

    #[test]
    fn reserved_words_block_users_but_not_tunnel_names() {
        // 保留字当用户名不行——会产出 admin-xxx 这种像官方的地址
        assert_eq!(validate_user("admin"), Err(code::NAME_RESERVED));
        assert_eq!(validate_user("www"), Err(code::NAME_RESERVED));
        // 但当隧道名完全可以：zhangsan-api 不会冒充 api.t.example.com
        assert!(validate_name("admin").is_ok());
        assert!(validate_name("api").is_ok());
    }

    #[test]
    fn composed_host_can_never_collide_with_a_reserved_word() {
        // 这是「隧道名不查保留字」的依据：拼出来的标签必然含连字符，保留字都不含
        for word in RESERVED {
            assert!(
                !word.contains('-'),
                "保留字 {word} 不该含连字符，否则可能与拼装标签相撞"
            );
        }
        assert_eq!(
            host_for("zhangsan", "api", "t.example.com"),
            "zhangsan-api.t.example.com"
        );
    }

    #[test]
    fn strips_port_but_keeps_ipv6() {
        assert_eq!(host_without_port("a.example.com:8080"), "a.example.com");
        assert_eq!(host_without_port("a.example.com"), "a.example.com");
        assert_eq!(host_without_port("[::1]:8080"), "[::1]");
        assert_eq!(host_without_port("[::1]"), "[::1]");
    }
}
