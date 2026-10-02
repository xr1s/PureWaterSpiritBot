//! 识别消息里的链接，跟随跳转得到最终地址，并去掉地址里的追踪参数。

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    time::Duration,
};

use reqwest::{Client, header, redirect::Policy};
use teloxide::types::{Message, MessageEntity, MessageEntityKind, MessageEntityRef};
use url::Url;

/// 最多跟随多少次跳转。
const MAX_REDIRECTS: usize = 5;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// 有些站点对没有浏览器特征的请求会跳到验证页，所以带上常见的 User-Agent。
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
    (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

/// 消息是不是只含一条链接：整条消息的文字就是这条链接，没有其他内容。
pub fn single_link(message: &Message) -> Option<Url> {
    single_link_in(message.text()?, message.entities()?)
}

fn single_link_in(text: &str, entities: &[MessageEntity]) -> Option<Url> {
    let parsed = MessageEntityRef::parse(text, entities);
    let [entity] = parsed.as_slice() else {
        return None;
    };
    if *entity.kind() != MessageEntityKind::Url || entity.text() != text.trim() {
        return None;
    }
    parse_web_url(entity.text())
}

/// 解析 http(s) 链接。Telegram 会把没有协议头的 `example.com/a` 也识别成链接，这里补上 `https://`。
fn parse_web_url(text: &str) -> Option<Url> {
    let url = match Url::parse(text) {
        Ok(url) => url,
        Err(url::ParseError::RelativeUrlWithoutBase) => {
            Url::parse(&format!("https://{text}")).ok()?
        }
        Err(_) => return None,
    };
    matches!(url.scheme(), "http" | "https").then_some(url)
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ResolveError {
    /// 地址指向本机或内网，或者协议不是 http(s)。
    #[error("the address is not a public web address: {0}")]
    Blocked(String),
    /// 没能走完跳转，`last` 是已经到达的最后一个地址。
    #[error("failed to follow the link: {reason}")]
    Failed { last: Url, reason: String },
}

/// 跟随 HTTP 跳转的客户端。
///
/// 每一跳都会检查目标是不是公网地址，避免被链接带去访问本机或内网的服务。
/// 这只能拦住这一步自己发出的请求：之后下载工具还会再发请求，
/// 那一步要靠部署时限制出站访问来兜底。
pub struct Resolver {
    client: Client,
    allow_private: bool,
}

impl Resolver {
    pub fn new() -> Self {
        Self::build(false)
    }

    #[cfg(test)]
    pub(super) fn allowing_private_addresses() -> Self {
        Self::build(true)
    }

    fn build(allow_private: bool) -> Self {
        let client = Client::builder()
            .redirect(Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .expect("HTTP client configuration is valid");
        Self {
            client,
            allow_private,
        }
    }

    /// 跟随跳转，返回最终地址。只读响应头，不下载内容。
    pub async fn resolve_redirects(&self, url: Url) -> Result<Url, ResolveError> {
        let mut current = url;
        for _ in 0..=MAX_REDIRECTS {
            self.ensure_public(&current).await?;
            let response = self
                .client
                .get(current.clone())
                .send()
                .await
                .map_err(|error| ResolveError::Failed {
                    last: current.clone(),
                    reason: error.to_string(),
                })?;
            if !response.status().is_redirection() {
                return Ok(current);
            }
            let next = response
                .headers()
                .get(header::LOCATION)
                .and_then(|value| value.to_str().ok())
                .and_then(|location| current.join(location).ok());
            match next {
                Some(next) => current = next,
                None => return Ok(current),
            }
        }
        Err(ResolveError::Failed {
            last: current,
            reason: "too many redirects".to_owned(),
        })
    }

    async fn ensure_public(&self, url: &Url) -> Result<(), ResolveError> {
        if !matches!(url.scheme(), "http" | "https") {
            return Err(ResolveError::Blocked(url.to_string()));
        }
        if self.allow_private {
            return Ok(());
        }
        let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default()) else {
            return Err(ResolveError::Blocked(url.to_string()));
        };
        let failed = |reason: String| ResolveError::Failed {
            last: url.clone(),
            reason,
        };
        let addresses: Vec<_> = tokio::net::lookup_host((host, port))
            .await
            .map_err(|error| failed(error.to_string()))?
            .collect();
        if addresses.is_empty() {
            return Err(failed("the host has no address".to_owned()));
        }
        if addresses.iter().all(|address| is_public(address.ip())) {
            Ok(())
        } else {
            Err(ResolveError::Blocked(url.to_string()))
        }
    }
}

/// 地址是不是公网上的单播地址。
fn is_public(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => match address.to_ipv4_mapped() {
            Some(mapped) => is_public_v4(mapped),
            None => is_public_v6(address),
        },
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    let carrier_grade_nat = a == 100 && (64..128).contains(&b);
    let benchmarking = a == 198 && (18..20).contains(&b);
    let protocol_assignments = a == 192 && b == 0 && c == 0;
    let reserved = a >= 240;
    let this_network = a == 0;
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_multicast()
        || carrier_grade_nat
        || benchmarking
        || protocol_assignments
        || reserved
        || this_network)
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    let first = address.segments()[0];
    let unique_local = first & 0xfe00 == 0xfc00;
    let link_local = first & 0xffc0 == 0xfe80;
    !(address.is_unspecified()
        || address.is_loopback()
        || address.is_multicast()
        || unique_local
        || link_local)
}

/// 任何站点都适用的追踪参数：前缀和名字。
const TRACKING_PREFIXES: &[&str] = &["utm_"];
const TRACKING_NAMES: &[&str] = &[
    "fbclid", "gclid", "dclid", "gbraid", "wbraid", "msclkid", "yclid", "twclid", "ttclid",
    "igshid", "mc_cid", "mc_eid", "mkt_tok", "_ga", "_gl",
];

/// 只在特定站点上才是追踪参数的名字。这些名字在别的站点上可能有用
/// （比如 GitHub 的 `ref` 是分支名），所以不能放进通用部分。
/// 凭证类的参数（比如小红书的 `xsec_token`）不在其中，删掉可能导致打不开。
const SITE_TRACKING_NAMES: &[(&[&str], &[&str])] = &[
    (
        &["facebook.com", "fb.com", "fb.watch"],
        &[
            "mibextid",
            "ref",
            "fref",
            "hc_ref",
            "sfnsn",
            "rdid",
            "__tn__",
            "__cft__[0]",
            "_rdr",
        ],
    ),
    (&["instagram.com"], &["igsh"]),
    (&["x.com", "twitter.com"], &["s", "t", "ref_src", "ref_url"]),
    (&["youtube.com", "youtu.be"], &["si", "feature", "pp"]),
    (
        &["bilibili.com", "b23.tv"],
        &[
            "spm_id_from",
            "from_spmid",
            "vd_source",
            "share_source",
            "share_medium",
            "share_plat",
            "share_session_id",
            "share_tag",
            "unique_k",
            "bbid",
            "buvid",
        ],
    ),
    (
        &["xiaohongshu.com", "xhslink.com"],
        &[
            "xhsshare",
            "appuid",
            "apptime",
            "share_id",
            "shareRedId",
            "share_from_user_hidden",
        ],
    ),
    (
        &["reddit.com", "redd.it"],
        &[
            "share_id",
            "rdt",
            "ref",
            "ref_source",
            "ref_campaign",
            "correlation_id",
            "$deep_link",
        ],
    ),
    (
        &["tiktok.com"],
        &[
            "_r",
            "_t",
            "is_from_webapp",
            "sender_device",
            "sender_web_id",
            "share_app_id",
            "share_item_id",
            "share_link_id",
            "social_sharing",
            "tt_from",
            "refer",
        ],
    ),
];

/// 去掉地址里已知的追踪参数，其余参数原样保留。
/// 只删黑名单里的参数，所以不会误删某个站点打开页面所需要的参数。
pub fn strip_tracking_params(url: &Url) -> Url {
    let Some(query) = url.query() else {
        return url.clone();
    };
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    let site_names: Vec<&str> = SITE_TRACKING_NAMES
        .iter()
        .filter(|(domains, _)| {
            domains
                .iter()
                .any(|domain| is_host_or_subdomain(&host, domain))
        })
        .flat_map(|(_, names)| names.iter().copied())
        .collect();
    let kept: Vec<&str> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter(|pair| {
            let name = url::form_urlencoded::parse(pair.as_bytes())
                .next()
                .map(|(name, _)| name.into_owned())
                .unwrap_or_default();
            let lowercase = name.to_ascii_lowercase();
            let tracked = TRACKING_PREFIXES
                .iter()
                .any(|prefix| lowercase.starts_with(prefix))
                || TRACKING_NAMES.contains(&lowercase.as_str())
                || site_names.contains(&name.as_str());
            !tracked
        })
        .collect();
    let mut stripped = url.clone();
    if kept.is_empty() {
        stripped.set_query(None);
    } else {
        stripped.set_query(Some(&kept.join("&")));
    }
    stripped
}

fn is_host_or_subdomain(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

#[cfg(test)]
mod tests {
    use teloxide::types::MessageEntity;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;

    fn url(text: &str) -> Url {
        Url::parse(text).unwrap()
    }

    fn stripped(text: &str) -> String {
        strip_tracking_params(&url(text)).to_string()
    }

    #[test]
    fn strips_generic_tracking_parameters() {
        assert_eq!(
            stripped("https://example.com/a?id=7&utm_source=x&utm_medium=y&fbclid=abc"),
            "https://example.com/a?id=7"
        );
        assert_eq!(
            stripped("https://example.com/a?utm_source=x&gclid=1"),
            "https://example.com/a"
        );
        assert_eq!(
            stripped("https://example.com/a?UTM_Source=x"),
            "https://example.com/a"
        );
    }

    #[test]
    fn keeps_parameters_a_site_needs() {
        assert_eq!(
            stripped("https://www.youtube.com/watch?v=abc&list=PL1&t=30&si=zzz&feature=shared"),
            "https://www.youtube.com/watch?v=abc&list=PL1&t=30"
        );
        assert_eq!(
            stripped("https://example.com/a?v=1&page=2"),
            "https://example.com/a?v=1&page=2"
        );
        assert_eq!(
            stripped(
                "https://www.xiaohongshu.com/explore/1?xsec_token=T&xsec_source=pc_feed&xhsshare=CopyLink"
            ),
            "https://www.xiaohongshu.com/explore/1?xsec_token=T&xsec_source=pc_feed"
        );
    }

    #[test]
    fn site_parameters_only_apply_to_that_site() {
        assert_eq!(
            stripped("https://github.com/o/r/blob/main/a?ref=main"),
            "https://github.com/o/r/blob/main/a?ref=main"
        );
        assert_eq!(
            stripped("https://www.facebook.com/share/p/1/?mibextid=abc&ref=x"),
            "https://www.facebook.com/share/p/1/"
        );
        assert_eq!(
            stripped("https://x.com/user/status/1?s=20&t=abc"),
            "https://x.com/user/status/1"
        );
        assert_eq!(
            stripped("https://example.com/a?s=20&t=abc"),
            "https://example.com/a?s=20&t=abc"
        );
        assert_eq!(
            stripped("https://notfacebook.com/a?mibextid=1"),
            "https://notfacebook.com/a?mibextid=1"
        );
    }

    #[test]
    fn leaves_other_parts_of_the_address_alone() {
        assert_eq!(
            stripped("https://example.com/a#top"),
            "https://example.com/a#top"
        );
        assert_eq!(
            stripped("https://example.com/a?utm_source=x#top"),
            "https://example.com/a#top"
        );
        assert_eq!(
            stripped("https://example.com/a?q=a%20b&utm_source=x"),
            "https://example.com/a?q=a%20b"
        );
    }

    fn entity(offset: usize, length: usize, kind: MessageEntityKind) -> MessageEntity {
        MessageEntity {
            kind,
            offset,
            length,
        }
    }

    #[test]
    fn recognizes_a_message_that_is_only_a_link() {
        let text = "https://example.com/a?b=1";
        let entities = [entity(0, text.len(), MessageEntityKind::Url)];
        assert_eq!(single_link_in(text, &entities), Some(url(text)));

        let padded = format!("  {text}\n");
        let entities = [entity(2, text.len(), MessageEntityKind::Url)];
        assert_eq!(single_link_in(&padded, &entities), Some(url(text)));
    }

    #[test]
    fn a_link_without_a_scheme_is_assumed_to_be_https() {
        let text = "example.com/a";
        let entities = [entity(0, text.len(), MessageEntityKind::Url)];
        assert_eq!(
            single_link_in(text, &entities),
            Some(url("https://example.com/a"))
        );
    }

    #[test]
    fn a_link_with_other_content_is_not_a_fetch_request() {
        let text = "look https://example.com/a";
        let entities = [entity(5, 21, MessageEntityKind::Url)];
        assert_eq!(single_link_in(text, &entities), None);

        let text = "https://example.com/a https://example.com/b";
        let entities = [
            entity(0, 21, MessageEntityKind::Url),
            entity(22, 21, MessageEntityKind::Url),
        ];
        assert_eq!(single_link_in(text, &entities), None);

        assert_eq!(single_link_in("hello", &[]), None);

        let text = "https://example.com/a";
        let entities = [entity(0, text.len(), MessageEntityKind::Bold)];
        assert_eq!(single_link_in(text, &entities), None);

        let text = "ftp://example.com/a";
        let entities = [entity(0, text.len(), MessageEntityKind::Url)];
        assert_eq!(single_link_in(text, &entities), None);
    }

    #[test]
    fn classifies_addresses() {
        for private in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "0.0.0.0",
            "224.0.0.1",
            "::1",
            "fe80::1",
            "fd00::1",
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
        ] {
            assert!(!is_public(private.parse().unwrap()), "{private}");
        }
        for public in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_public(public.parse().unwrap()), "{public}");
        }
    }

    #[tokio::test]
    async fn follows_redirects_to_the_final_address() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/short"))
            .respond_with(ResponseTemplate::new(301).insert_header("location", "/middle"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/middle"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "/final?utm_source=x"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/final"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let resolver = Resolver::allowing_private_addresses();
        let resolved = resolver
            .resolve_redirects(url(&format!("{}/short", server.uri())))
            .await
            .unwrap();
        assert_eq!(resolved.path(), "/final");
        assert_eq!(resolved.query(), Some("utm_source=x"));
    }

    #[tokio::test]
    async fn an_address_that_does_not_redirect_is_returned_as_is() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let start = url(&format!("{}/a?b=1", server.uri()));
        let resolved = Resolver::allowing_private_addresses()
            .resolve_redirects(start.clone())
            .await
            .unwrap();
        assert_eq!(resolved, start);
    }

    #[tokio::test]
    async fn gives_up_on_a_redirect_loop() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", "/loop"))
            .mount(&server)
            .await;
        let error = Resolver::allowing_private_addresses()
            .resolve_redirects(url(&format!("{}/loop", server.uri())))
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::Failed { .. }));
    }

    #[tokio::test]
    async fn refuses_local_and_non_web_addresses() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let resolver = Resolver::new();
        let error = resolver
            .resolve_redirects(url(&format!("{}/a", server.uri())))
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::Blocked(_)));
        let error = resolver
            .resolve_redirects(url("file:///etc/passwd"))
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::Blocked(_)));
    }
}
