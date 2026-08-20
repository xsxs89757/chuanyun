//! 错误码与中文人话映射。
//!
//! 协议里传 `code`，界面展示 `human()`——服务端不承担文案职责，客户端也不猜测语义。

/// 错误码（协议内传输的稳定字符串）。
pub mod code {
    pub const AUTH_INVALID: &str = "E_AUTH_INVALID";
    pub const AUTH_EXPIRED: &str = "E_AUTH_EXPIRED";
    pub const AUTH_REVOKED: &str = "E_AUTH_REVOKED";
    pub const VERSION: &str = "E_VERSION";
    pub const SUBDOMAIN_TAKEN: &str = "E_SUBDOMAIN_TAKEN";
    pub const NAME_RESERVED: &str = "E_NAME_RESERVED";
    pub const NAME_INVALID: &str = "E_NAME_INVALID";
    pub const PORT_TAKEN: &str = "E_PORT_TAKEN";
    pub const POOL_EXHAUSTED: &str = "E_POOL_EXHAUSTED";
    pub const LIMIT: &str = "E_LIMIT";
    pub const LOCKED: &str = "E_LOCKED";
    pub const INTERNAL: &str = "E_INTERNAL";
}

/// 把错误码翻译成给人看的中文。未知码回退为原样展示，不吞掉信息。
pub fn human(code: &str) -> &str {
    match code {
        self::code::AUTH_INVALID => "凭证无效，请检查后重新登录",
        self::code::AUTH_EXPIRED => "凭证已过期，请联系管理员重新发放",
        self::code::AUTH_REVOKED => "凭证已被吊销，请联系管理员",
        self::code::VERSION => "客户端版本过旧，请到下载页更新",
        self::code::SUBDOMAIN_TAKEN => "该隧道名已被占用，请换一个名称",
        self::code::NAME_RESERVED => "该名称是保留字，请换一个名称",
        self::code::NAME_INVALID => "名称只能用小写字母、数字和连字符，且不能以连字符开头或结尾",
        self::code::PORT_TAKEN => "指定的公网端口已被占用",
        self::code::POOL_EXHAUSTED => "公网端口池已满，请联系管理员",
        self::code::LIMIT => "已达到隧道数量上限",
        self::code::LOCKED => "失败次数过多，请稍后再试",
        self::code::INTERNAL => "服务端内部错误，请联系管理员",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codes_are_translated() {
        assert_eq!(human(code::AUTH_REVOKED), "凭证已被吊销，请联系管理员");
    }

    #[test]
    fn unknown_code_passes_through() {
        assert_eq!(human("E_SOMETHING_NEW"), "E_SOMETHING_NEW");
    }
}
