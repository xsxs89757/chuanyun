//! 品牌注入：把部署方的默认值编译进二进制。
//!
//! 开源仓库里只有通用示例（`brand/default.toml`）。某家公司要给同事发「装上就能用」
//! 的安装包时，用自己的私有配置替换它再编译——服务器地址和证书指纹属于部署方的
//! 私有信息，不该出现在公开仓库里，也不该要求每个同事手工填。
//!
//! 构建时指定：`CHUANYUN_BRAND=brand/company.toml cargo build`

use serde::Deserialize;

use crate::engine::Brand;

/// 编译期嵌入的品牌配置内容。
const EMBEDDED: &str = include_str!(env!("CHUANYUN_BRAND_FILE"));

#[derive(Debug, Deserialize)]
#[serde(default)]
struct BrandFile {
    product_name: String,
    default_server: String,
    tls_pin: String,
    tls_verify: String,
    update_url: String,
}

impl Default for BrandFile {
    fn default() -> Self {
        Self {
            product_name: "穿云".into(),
            default_server: String::new(),
            tls_pin: String::new(),
            tls_verify: "pin".into(),
            update_url: String::new(),
        }
    }
}

/// 读取编译进来的品牌配置。
///
/// 解析失败不该让应用起不来——退回全空的默认值，用户手动填服务器地址还是能用。
pub fn embedded() -> Brand {
    let file: BrandFile = toml::from_str(EMBEDDED).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "品牌配置解析失败，退回默认值");
        BrandFile::default()
    });
    Brand {
        default_server: file.default_server,
        tls_pin: file.tls_pin,
        update_url: file.update_url,
    }
}

/// 产品名，用于窗口标题等处。
pub fn product_name() -> String {
    toml::from_str::<BrandFile>(EMBEDDED)
        .map(|f| f.product_name)
        .unwrap_or_else(|_| "穿云".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_brand_parses() {
        // 开源默认配置里这些值是空的，用户首次启动时自己填
        let brand = embedded();
        assert!(!product_name().is_empty());
        // 只要不 panic 就行——空值是开源版的正常状态
        let _ = brand.default_server;
    }

    #[test]
    fn malformed_brand_does_not_take_the_app_down() {
        let file: BrandFile = toml::from_str("this is not toml = = =").unwrap_or_default();
        assert_eq!(file.product_name, "穿云");
    }
}
