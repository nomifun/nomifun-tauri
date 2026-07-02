use std::collections::HashSet;

use tracing::{debug, warn};

const PROXY_ENV_KEYS: &[&str] = &[
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct SystemProxyConfig {
    http_proxy: Option<String>,
    https_proxy: Option<String>,
    all_proxy: Option<String>,
    no_proxy: Option<String>,
}

pub fn apply_detected_proxy(mut builder: reqwest::ClientBuilder) -> reqwest::ClientBuilder {
    if process_has_proxy_env() {
        return builder;
    }

    let Some(config) = system_proxy_config() else {
        return builder;
    };

    let no_proxy = effective_no_proxy(&config);

    if let Some(proxy_url) = config.all_proxy.as_deref() {
        match reqwest::Proxy::all(proxy_url) {
            Ok(proxy) => {
                debug!("Using detected system ALL_PROXY for outbound HTTP client");
                return builder.proxy(proxy.no_proxy(no_proxy));
            }
            Err(err) => warn!(error = %err, "Ignoring invalid detected system ALL_PROXY"),
        }
    }

    if let Some(proxy_url) = config.http_proxy.as_deref() {
        match reqwest::Proxy::http(proxy_url) {
            Ok(proxy) => {
                debug!("Using detected system HTTP_PROXY for outbound HTTP client");
                builder = builder.proxy(proxy.no_proxy(no_proxy.clone()));
            }
            Err(err) => warn!(error = %err, "Ignoring invalid detected system HTTP_PROXY"),
        }
    }

    if let Some(proxy_url) = config.https_proxy.as_deref() {
        match reqwest::Proxy::https(proxy_url) {
            Ok(proxy) => {
                debug!("Using detected system HTTPS_PROXY for outbound HTTP client");
                builder = builder.proxy(proxy.no_proxy(no_proxy));
            }
            Err(err) => warn!(error = %err, "Ignoring invalid detected system HTTPS_PROXY"),
        }
    }

    builder
}

pub fn child_proxy_env<'a, I>(configured_env_names: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = &'a str>,
{
    let configured_names: HashSet<String> = configured_env_names
        .into_iter()
        .map(|name| name.to_ascii_uppercase())
        .collect();
    let process_names = process_env_proxy_names();
    if has_proxy_name(&configured_names) || has_proxy_name(&process_names) {
        return Vec::new();
    }

    let Some(config) = system_proxy_config() else {
        return Vec::new();
    };

    proxy_env_from_config(&config, &configured_names, &process_names)
}

fn system_proxy_config() -> Option<SystemProxyConfig> {
    detect_system_proxy()
}

fn process_has_proxy_env() -> bool {
    has_proxy_name(&process_env_proxy_names())
}

fn process_env_proxy_names() -> HashSet<String> {
    std::env::vars()
        .filter(|(_, value)| !value.trim().is_empty())
        .map(|(name, _)| name.to_ascii_uppercase())
        .collect()
}

fn has_proxy_name(names: &HashSet<String>) -> bool {
    PROXY_ENV_KEYS
        .iter()
        .any(|key| names.contains(&key.to_ascii_uppercase()))
}

fn has_env_name(names: &HashSet<String>, key: &str) -> bool {
    names.contains(&key.to_ascii_uppercase())
}

fn proxy_env_from_config(
    config: &SystemProxyConfig,
    configured_names: &HashSet<String>,
    process_names: &HashSet<String>,
) -> Vec<(String, String)> {
    let mut vars = Vec::new();
    push_proxy_pair(
        &mut vars,
        "HTTP_PROXY",
        config.http_proxy.as_deref(),
        configured_names,
        process_names,
    );
    push_proxy_pair(
        &mut vars,
        "HTTPS_PROXY",
        config.https_proxy.as_deref(),
        configured_names,
        process_names,
    );
    push_proxy_pair(
        &mut vars,
        "ALL_PROXY",
        config.all_proxy.as_deref(),
        configured_names,
        process_names,
    );

    if let Some(no_proxy) = config.no_proxy.as_deref() {
        push_if_missing(
            &mut vars,
            "NO_PROXY",
            no_proxy,
            configured_names,
            process_names,
        );
        push_if_missing(
            &mut vars,
            "no_proxy",
            no_proxy,
            configured_names,
            process_names,
        );
    }

    vars
}

fn push_proxy_pair(
    vars: &mut Vec<(String, String)>,
    upper_key: &str,
    value: Option<&str>,
    configured_names: &HashSet<String>,
    process_names: &HashSet<String>,
) {
    let Some(value) = value else {
        return;
    };
    push_if_missing(vars, upper_key, value, configured_names, process_names);
    push_if_missing(
        vars,
        &upper_key.to_ascii_lowercase(),
        value,
        configured_names,
        process_names,
    );
}

fn push_if_missing(
    vars: &mut Vec<(String, String)>,
    key: &str,
    value: &str,
    configured_names: &HashSet<String>,
    process_names: &HashSet<String>,
) {
    if has_env_name(configured_names, key) || has_env_name(process_names, key) {
        return;
    }
    vars.push((key.to_owned(), value.to_owned()));
}

fn effective_no_proxy(config: &SystemProxyConfig) -> Option<reqwest::NoProxy> {
    let mut items = Vec::new();
    if let Ok(value) = std::env::var("NO_PROXY").or_else(|_| std::env::var("no_proxy"))
        && !value.trim().is_empty()
    {
        items.push(value);
    }
    if let Some(value) = config.no_proxy.as_ref()
        && !value.trim().is_empty()
    {
        items.push(value.clone());
    }

    if items.is_empty() {
        return None;
    }
    reqwest::NoProxy::from_string(&items.join(","))
}

fn detect_system_proxy() -> Option<SystemProxyConfig> {
    #[cfg(test)]
    if let Some(config) = take_test_system_proxy_config() {
        return config;
    }

    detect_platform_proxy()
}

#[cfg(test)]
static TEST_SYSTEM_PROXY_CONFIGS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::VecDeque<Option<SystemProxyConfig>>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::VecDeque::new()));

#[cfg(test)]
fn set_test_system_proxy_configs(configs: Vec<Option<SystemProxyConfig>>) {
    let mut guard = TEST_SYSTEM_PROXY_CONFIGS
        .lock()
        .expect("test system proxy config lock");
    guard.clear();
    guard.extend(configs);
}

#[cfg(test)]
fn take_test_system_proxy_config() -> Option<Option<SystemProxyConfig>> {
    TEST_SYSTEM_PROXY_CONFIGS
        .lock()
        .expect("test system proxy config lock")
        .pop_front()
}

#[cfg(target_os = "macos")]
fn detect_platform_proxy() -> Option<SystemProxyConfig> {
    use std::process::Command;

    let output = Command::new("/usr/sbin/scutil")
        .arg("--proxy")
        .output()
        .or_else(|_| Command::new("scutil").arg("--proxy").output())
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_scutil_proxy(&stdout)
}

#[cfg(target_os = "windows")]
fn detect_platform_proxy() -> Option<SystemProxyConfig> {
    let proxy_enable = read_windows_internet_settings("ProxyEnable")
        .and_then(|value| parse_windows_proxy_enable(&value));
    let proxy_server = read_windows_internet_settings("ProxyServer").unwrap_or_default();
    let proxy_override = read_windows_internet_settings("ProxyOverride");

    parse_windows_proxy_settings(
        proxy_enable.unwrap_or(false),
        &proxy_server,
        proxy_override.as_deref(),
    )
}

#[cfg(target_os = "windows")]
fn read_windows_internet_settings(name: &str) -> Option<String> {
    use std::process::Command;

    let output = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows\CurrentVersion\Internet Settings",
            "/v",
            name,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_windows_reg_value(&stdout, name)
}

#[cfg(target_os = "windows")]
fn parse_windows_reg_value(text: &str, name: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        let mut parts = line.split_whitespace();
        let Some(value_name) = parts.next() else {
            continue;
        };
        if !value_name.eq_ignore_ascii_case(name) {
            continue;
        }
        parts.next()?;
        let value = parts.collect::<Vec<_>>().join(" ");
        return non_empty(&value);
    }

    None
}

#[cfg(target_os = "windows")]
fn parse_windows_proxy_enable(value: &str) -> Option<bool> {
    let value = value.trim();
    if let Some(hex) = value
        .strip_prefix("0x")
        .or_else(|| value.strip_prefix("0X"))
    {
        return u32::from_str_radix(hex, 16).ok().map(|value| value != 0);
    }
    if value.eq_ignore_ascii_case("true") {
        return Some(true);
    }
    if value.eq_ignore_ascii_case("false") {
        return Some(false);
    }
    value.parse::<u32>().ok().map(|value| value != 0)
}

#[cfg(target_os = "linux")]
fn detect_platform_proxy() -> Option<SystemProxyConfig> {
    detect_linux_gsettings_proxy().or_else(detect_linux_kde_proxy)
}

#[cfg(target_os = "linux")]
fn detect_linux_gsettings_proxy() -> Option<SystemProxyConfig> {
    let mode = read_gsettings_value("org.gnome.system.proxy", "mode")?;
    let settings = LinuxGSettingsProxy {
        mode,
        http_host: read_gsettings_value("org.gnome.system.proxy.http", "host"),
        http_port: read_gsettings_value("org.gnome.system.proxy.http", "port"),
        https_host: read_gsettings_value("org.gnome.system.proxy.https", "host"),
        https_port: read_gsettings_value("org.gnome.system.proxy.https", "port"),
        socks_host: read_gsettings_value("org.gnome.system.proxy.socks", "host"),
        socks_port: read_gsettings_value("org.gnome.system.proxy.socks", "port"),
        ignore_hosts: read_gsettings_value("org.gnome.system.proxy", "ignore-hosts"),
    };

    parse_linux_gsettings_proxy(settings)
}

#[cfg(target_os = "linux")]
fn detect_linux_kde_proxy() -> Option<SystemProxyConfig> {
    let settings = LinuxKdeProxy {
        proxy_type: read_kde_proxy_value("ProxyType")?,
        http_proxy: read_kde_proxy_value("httpProxy"),
        https_proxy: read_kde_proxy_value("httpsProxy"),
        socks_proxy: read_kde_proxy_value("socksProxy")
            .or_else(|| read_kde_proxy_value("sockProxy")),
        no_proxy: read_kde_proxy_value("NoProxyFor"),
    };

    parse_linux_kde_proxy(settings)
}

#[cfg(target_os = "linux")]
fn read_gsettings_value(schema: &str, key: &str) -> Option<String> {
    use std::process::Command;

    let output = Command::new("gsettings")
        .args(["get", schema, key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    non_empty(&stdout)
}

#[cfg(target_os = "linux")]
fn read_kde_proxy_value(key: &str) -> Option<String> {
    use std::process::Command;

    for command in ["kreadconfig6", "kreadconfig5"] {
        let output = Command::new(command)
            .args([
                "--file",
                "kioslaverc",
                "--group",
                "Proxy Settings",
                "--key",
                key,
            ])
            .output();
        let Ok(output) = output else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        if let Some(value) = non_empty(&stdout) {
            return Some(value);
        }
    }

    None
}

#[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
fn detect_platform_proxy() -> Option<SystemProxyConfig> {
    None
}

#[cfg(target_os = "macos")]
fn parse_scutil_proxy(text: &str) -> Option<SystemProxyConfig> {
    let mut http_enable = false;
    let mut https_enable = false;
    let mut socks_enable = false;
    let mut http_host = None;
    let mut https_host = None;
    let mut socks_host = None;
    let mut http_port = None;
    let mut https_port = None;
    let mut socks_port = None;
    let mut exceptions = Vec::new();
    let mut in_exceptions = false;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.starts_with("ExceptionsList") && line.contains("<array>") {
            in_exceptions = true;
            continue;
        }
        if in_exceptions {
            if line.starts_with('}') {
                in_exceptions = false;
                continue;
            }
            if let Some((_, value)) = line.split_once(':') {
                exceptions.extend(parse_exception_values(value));
            }
            continue;
        }

        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "HTTPEnable" => http_enable = value == "1",
            "HTTPSEnable" => https_enable = value == "1",
            "SOCKSEnable" => socks_enable = value == "1",
            "HTTPProxy" => http_host = non_empty(value),
            "HTTPSProxy" => https_host = non_empty(value),
            "SOCKSProxy" => socks_host = non_empty(value),
            "HTTPPort" => http_port = parse_port(value),
            "HTTPSPort" => https_port = parse_port(value),
            "SOCKSPort" => socks_port = parse_port(value),
            _ => {}
        }
    }

    let http_proxy = enabled_proxy_url(http_enable, "http", http_host.as_deref(), http_port);
    let https_proxy = enabled_proxy_url(https_enable, "http", https_host.as_deref(), https_port);
    let all_proxy = if http_proxy.is_none() && https_proxy.is_none() && socks_enable {
        enabled_proxy_url(true, "socks5h", socks_host.as_deref(), socks_port)
    } else {
        None
    };

    if http_proxy.is_none() && https_proxy.is_none() && all_proxy.is_none() {
        return None;
    }

    Some(SystemProxyConfig {
        http_proxy,
        https_proxy,
        all_proxy,
        no_proxy: build_no_proxy(exceptions),
    })
}

#[cfg(any(test, target_os = "macos", target_os = "linux"))]
fn enabled_proxy_url(
    enabled: bool,
    scheme: &str,
    host: Option<&str>,
    port: Option<u16>,
) -> Option<String> {
    if !enabled {
        return None;
    }
    proxy_url(scheme, host?, port?)
}

fn non_empty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_owned())
}

fn parse_port(value: &str) -> Option<u16> {
    value.trim().parse().ok()
}

fn proxy_url(scheme: &str, host: &str, port: u16) -> Option<String> {
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    let host = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_owned()
    };
    Some(format!("{scheme}://{host}:{port}"))
}

#[cfg(target_os = "macos")]
fn parse_exception_values(value: &str) -> Vec<String> {
    value
        .split(',')
        .filter_map(|item| {
            let item = item.trim();
            if item.is_empty() {
                return None;
            }
            Some(item.to_owned())
        })
        .collect()
}

fn build_no_proxy(exceptions: Vec<String>) -> Option<String> {
    let mut items = vec![
        "localhost".to_owned(),
        "127.0.0.1".to_owned(),
        "::1".to_owned(),
        "10.0.0.0/8".to_owned(),
        "127.0.0.0/8".to_owned(),
        "172.16.0.0/12".to_owned(),
        "192.168.0.0/16".to_owned(),
    ];
    for item in exceptions {
        if let Some(normalized) = normalize_no_proxy_item(&item) {
            items.push(normalized);
        }
    }
    items.sort();
    items.dedup();

    (!items.is_empty()).then(|| items.join(","))
}

fn normalize_no_proxy_item(item: &str) -> Option<String> {
    let item = item.trim().trim_matches('\'').trim_matches('"');
    if item.is_empty() {
        return None;
    }
    if item.eq_ignore_ascii_case("<local>") {
        return Some("localhost".to_owned());
    }
    match item {
        "127.*" => return Some("127.0.0.0/8".to_owned()),
        "10.*" => return Some("10.0.0.0/8".to_owned()),
        "192.168.*" => return Some("192.168.0.0/16".to_owned()),
        _ => {}
    }
    if item.starts_with("172.") {
        return Some("172.16.0.0/12".to_owned());
    }
    if let Some(domain) = item.strip_prefix("*.") {
        return Some(format!(".{domain}"));
    }
    if let Some(domain) = item.strip_prefix('*') {
        return (!domain.is_empty()).then(|| domain.to_owned());
    }
    Some(item.to_owned())
}

#[cfg(any(test, target_os = "windows"))]
fn parse_windows_proxy_settings(
    proxy_enable: bool,
    proxy_server: &str,
    proxy_override: Option<&str>,
) -> Option<SystemProxyConfig> {
    if !proxy_enable {
        return None;
    }

    let proxy_server = proxy_server.trim();
    if proxy_server.is_empty() {
        return None;
    }

    let mut default_proxy = None;
    let mut http_proxy = None;
    let mut https_proxy = None;
    let mut socks_proxy = None;

    for segment in proxy_server.split(';') {
        let segment = segment.trim();
        if segment.is_empty() {
            continue;
        }

        let Some((key, value)) = segment.split_once('=') else {
            default_proxy = Some(segment);
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "http" => http_proxy = normalize_proxy_url("http", value),
            "https" => https_proxy = normalize_proxy_url("http", value),
            "socks" | "socks5" => socks_proxy = normalize_proxy_url("socks5h", value),
            _ => {}
        }
    }

    if let Some(proxy) = default_proxy {
        if http_proxy.is_none() {
            http_proxy = normalize_proxy_url("http", proxy);
        }
        if https_proxy.is_none() {
            https_proxy = normalize_proxy_url("http", proxy);
        }
    }

    let all_proxy = if http_proxy.is_none() && https_proxy.is_none() {
        socks_proxy
    } else {
        None
    };

    if http_proxy.is_none() && https_proxy.is_none() && all_proxy.is_none() {
        return None;
    }

    Some(SystemProxyConfig {
        http_proxy,
        https_proxy,
        all_proxy,
        no_proxy: build_no_proxy(parse_windows_proxy_override(proxy_override)),
    })
}

#[cfg(any(test, target_os = "windows"))]
fn parse_windows_proxy_override(value: Option<&str>) -> Vec<String> {
    let mut items = Vec::new();
    let Some(value) = value else {
        return items;
    };

    for item in value.split(';') {
        let item = item.trim();
        if item.eq_ignore_ascii_case("<local>") {
            items.push("localhost".to_owned());
            items.push("127.0.0.1".to_owned());
            items.push("::1".to_owned());
            items.push(".local".to_owned());
            continue;
        }
        if !item.is_empty() {
            items.push(item.to_owned());
        }
    }

    items
}

#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, Clone, Default)]
struct LinuxGSettingsProxy {
    mode: String,
    http_host: Option<String>,
    http_port: Option<String>,
    https_host: Option<String>,
    https_port: Option<String>,
    socks_host: Option<String>,
    socks_port: Option<String>,
    ignore_hosts: Option<String>,
}

#[cfg(any(test, target_os = "linux"))]
#[derive(Debug, Clone, Default)]
struct LinuxKdeProxy {
    proxy_type: String,
    http_proxy: Option<String>,
    https_proxy: Option<String>,
    socks_proxy: Option<String>,
    no_proxy: Option<String>,
}

#[cfg(any(test, target_os = "linux"))]
fn parse_linux_gsettings_proxy(settings: LinuxGSettingsProxy) -> Option<SystemProxyConfig> {
    if parse_gsettings_string(&settings.mode)?.as_str() != "manual" {
        return None;
    }

    let http_proxy = enabled_proxy_url(
        true,
        "http",
        settings
            .http_host
            .as_deref()
            .and_then(parse_gsettings_string)
            .as_deref(),
        settings.http_port.as_deref().and_then(parse_gsettings_port),
    );
    let https_proxy = enabled_proxy_url(
        true,
        "http",
        settings
            .https_host
            .as_deref()
            .and_then(parse_gsettings_string)
            .as_deref(),
        settings
            .https_port
            .as_deref()
            .and_then(parse_gsettings_port),
    );
    let socks_proxy = enabled_proxy_url(
        true,
        "socks5h",
        settings
            .socks_host
            .as_deref()
            .and_then(parse_gsettings_string)
            .as_deref(),
        settings
            .socks_port
            .as_deref()
            .and_then(parse_gsettings_port),
    );
    let all_proxy = if http_proxy.is_none() && https_proxy.is_none() {
        socks_proxy
    } else {
        None
    };

    if http_proxy.is_none() && https_proxy.is_none() && all_proxy.is_none() {
        return None;
    }

    let exceptions = settings
        .ignore_hosts
        .as_deref()
        .map(parse_gsettings_list)
        .unwrap_or_default();

    Some(SystemProxyConfig {
        http_proxy,
        https_proxy,
        all_proxy,
        no_proxy: build_no_proxy(exceptions),
    })
}

#[cfg(any(test, target_os = "linux"))]
fn parse_linux_kde_proxy(settings: LinuxKdeProxy) -> Option<SystemProxyConfig> {
    if settings.proxy_type.trim() != "1" {
        return None;
    }

    let http_proxy = settings
        .http_proxy
        .as_deref()
        .and_then(|value| normalize_linux_kde_proxy_url("http", value));
    let https_proxy = settings
        .https_proxy
        .as_deref()
        .and_then(|value| normalize_linux_kde_proxy_url("http", value));
    let socks_proxy = settings
        .socks_proxy
        .as_deref()
        .and_then(|value| normalize_linux_kde_proxy_url("socks5h", value));
    let all_proxy = if http_proxy.is_none() && https_proxy.is_none() {
        socks_proxy
    } else {
        None
    };

    if http_proxy.is_none() && https_proxy.is_none() && all_proxy.is_none() {
        return None;
    }

    Some(SystemProxyConfig {
        http_proxy,
        https_proxy,
        all_proxy,
        no_proxy: build_no_proxy(parse_linux_kde_no_proxy(settings.no_proxy.as_deref())),
    })
}

#[cfg(any(test, target_os = "linux"))]
fn parse_gsettings_string(value: &str) -> Option<String> {
    let value = value.trim();
    if value.starts_with('@') {
        return None;
    }
    let value = value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
        .or_else(|| {
            value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
        })
        .unwrap_or(value)
        .trim();
    non_empty(value)
}

#[cfg(any(test, target_os = "linux"))]
fn parse_gsettings_port(value: &str) -> Option<u16> {
    let port = value
        .split_whitespace()
        .last()
        .and_then(parse_port)
        .filter(|port| *port > 0)?;
    Some(port)
}

#[cfg(any(test, target_os = "linux"))]
fn parse_gsettings_list(value: &str) -> Vec<String> {
    let value = value.trim();
    if value.starts_with('@') {
        return Vec::new();
    }
    let value = value
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(value);

    value
        .split(',')
        .filter_map(parse_gsettings_string)
        .collect()
}

#[cfg(any(test, target_os = "linux"))]
fn parse_linux_kde_no_proxy(value: Option<&str>) -> Vec<String> {
    value
        .into_iter()
        .flat_map(|value| value.split([',', ';']))
        .filter_map(non_empty)
        .collect()
}

#[cfg(any(test, target_os = "linux"))]
fn normalize_linux_kde_proxy_url(default_scheme: &str, value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }

    if let Some((scheme, endpoint)) = value.split_once("://") {
        let scheme = normalize_proxy_scheme(scheme, default_scheme);
        if let Some(url) = proxy_url_from_space_separated_endpoint(&scheme, endpoint) {
            return Some(url);
        }
    }

    normalize_proxy_url(default_scheme, value)
}

#[cfg(any(test, target_os = "linux"))]
fn proxy_url_from_space_separated_endpoint(scheme: &str, endpoint: &str) -> Option<String> {
    let mut parts = endpoint.split_whitespace();
    let host = parts.next()?;
    let port = parts.next().and_then(parse_port)?;
    proxy_url(scheme, host, port)
}

#[cfg(any(test, target_os = "windows", target_os = "linux"))]
fn normalize_proxy_url(default_scheme: &str, value: &str) -> Option<String> {
    let value = value.trim().trim_matches('"').trim_matches('\'');
    if value.is_empty() {
        return None;
    }

    let (scheme, endpoint) = value
        .split_once("://")
        .map(|(scheme, endpoint)| (normalize_proxy_scheme(scheme, default_scheme), endpoint))
        .unwrap_or_else(|| {
            (
                normalize_proxy_scheme(default_scheme, default_scheme),
                value,
            )
        });
    let endpoint = endpoint.trim().trim_end_matches('/');
    let (host, port) = split_proxy_endpoint(endpoint)?;
    proxy_url(&scheme, host, port)
}

#[cfg(any(test, target_os = "windows", target_os = "linux"))]
fn normalize_proxy_scheme(scheme: &str, default_scheme: &str) -> String {
    match scheme.trim().to_ascii_lowercase().as_str() {
        "http" => "http".to_owned(),
        "https" => "https".to_owned(),
        "socks" | "socks5" | "socks5h" => "socks5h".to_owned(),
        "" => default_scheme.to_ascii_lowercase(),
        _ => default_scheme.to_ascii_lowercase(),
    }
}

#[cfg(any(test, target_os = "windows", target_os = "linux"))]
fn split_proxy_endpoint(endpoint: &str) -> Option<(&str, u16)> {
    let endpoint = endpoint.trim();
    if endpoint.is_empty() {
        return None;
    }
    if let Some(rest) = endpoint.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &endpoint[..=end + 1];
        let port = rest[end + 1..].strip_prefix(':').and_then(parse_port)?;
        return Some((host, port));
    }

    let (host, port) = endpoint.rsplit_once(':')?;
    let host = host.trim();
    if host.is_empty() {
        return None;
    }
    Some((host, parse_port(port)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_proxy_config_reads_current_detection_each_call() {
        let first = SystemProxyConfig {
            http_proxy: Some("http://127.0.0.1:7890".to_owned()),
            https_proxy: Some("http://127.0.0.1:7890".to_owned()),
            all_proxy: None,
            no_proxy: None,
        };
        let second = SystemProxyConfig {
            http_proxy: Some("http://127.0.0.1:7900".to_owned()),
            https_proxy: Some("http://127.0.0.1:7900".to_owned()),
            all_proxy: None,
            no_proxy: None,
        };

        set_test_system_proxy_configs(vec![Some(first), Some(second)]);

        let first_config = system_proxy_config().expect("first proxy config");
        assert_eq!(
            first_config.http_proxy.as_deref(),
            Some("http://127.0.0.1:7890")
        );

        let second_config = system_proxy_config().expect("second proxy config");
        assert_eq!(
            second_config.http_proxy.as_deref(),
            Some("http://127.0.0.1:7900")
        );
    }

    #[test]
    fn proxy_env_from_config_adds_upper_and_lowercase_keys() {
        let config = SystemProxyConfig {
            http_proxy: Some("http://127.0.0.1:7892".to_owned()),
            https_proxy: Some("http://127.0.0.1:7892".to_owned()),
            all_proxy: None,
            no_proxy: Some("localhost,127.0.0.1".to_owned()),
        };
        let vars = proxy_env_from_config(&config, &HashSet::new(), &HashSet::new());

        assert!(vars.contains(&("HTTP_PROXY".to_owned(), "http://127.0.0.1:7892".to_owned())));
        assert!(vars.contains(&("http_proxy".to_owned(), "http://127.0.0.1:7892".to_owned())));
        assert!(vars.contains(&("HTTPS_PROXY".to_owned(), "http://127.0.0.1:7892".to_owned())));
        assert!(vars.contains(&("NO_PROXY".to_owned(), "localhost,127.0.0.1".to_owned())));
    }

    #[test]
    fn proxy_env_from_config_respects_existing_names() {
        let config = SystemProxyConfig {
            http_proxy: Some("http://127.0.0.1:7892".to_owned()),
            https_proxy: Some("http://127.0.0.1:7892".to_owned()),
            all_proxy: None,
            no_proxy: Some("localhost,127.0.0.1".to_owned()),
        };
        let configured_names = HashSet::from(["HTTPS_PROXY".to_owned()]);
        let process_names = HashSet::from(["NO_PROXY".to_owned()]);

        let vars = proxy_env_from_config(&config, &configured_names, &process_names);

        assert!(!vars.iter().any(|(name, _)| name == "HTTPS_PROXY"));
        assert!(!vars.iter().any(|(name, _)| name == "https_proxy"));
        assert!(!vars.iter().any(|(name, _)| name == "NO_PROXY"));
        assert!(vars.contains(&("HTTP_PROXY".to_owned(), "http://127.0.0.1:7892".to_owned())));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn child_proxy_env_uses_current_macos_system_proxy_when_needed() {
        let Some(config) = detect_platform_proxy() else {
            return;
        };
        if config.http_proxy.is_none() && config.https_proxy.is_none() && config.all_proxy.is_none()
        {
            return;
        }

        let vars = child_proxy_env(std::iter::empty());

        if process_has_proxy_env() {
            assert!(vars.is_empty());
        } else {
            let values: HashSet<&str> = vars.iter().map(|(_, value)| value.as_str()).collect();
            if let Some(http_proxy) = config.http_proxy.as_deref() {
                assert!(values.contains(http_proxy));
            }
            if let Some(https_proxy) = config.https_proxy.as_deref() {
                assert!(values.contains(https_proxy));
            }
            if let Some(all_proxy) = config.all_proxy.as_deref() {
                assert!(values.contains(all_proxy));
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_scutil_proxy_extracts_http_https_and_exceptions() {
        let input = r#"<dictionary> {
  ExceptionsList : <array> {
    0 : *zhihu.com,*zhimg.com,localhost,*.local,127.*,10.*,172.16.*,192.168.*
  }
  HTTPEnable : 1
  HTTPPort : 7892
  HTTPProxy : 127.0.0.1
  HTTPSEnable : 1
  HTTPSPort : 7892
  HTTPSProxy : 127.0.0.1
  SOCKSEnable : 1
  SOCKSPort : 7892
  SOCKSProxy : 127.0.0.1
}"#;

        let config = parse_scutil_proxy(input).expect("proxy config");

        assert_eq!(config.http_proxy.as_deref(), Some("http://127.0.0.1:7892"));
        assert_eq!(config.https_proxy.as_deref(), Some("http://127.0.0.1:7892"));
        assert_eq!(config.all_proxy, None);
        let no_proxy = config.no_proxy.expect("no_proxy");
        assert!(no_proxy.contains("zhihu.com"));
        assert!(no_proxy.contains(".local"));
        assert!(no_proxy.contains("127.0.0.0/8"));
        assert!(no_proxy.contains("192.168.0.0/16"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn parse_scutil_proxy_uses_socks_when_http_is_absent() {
        let input = r#"<dictionary> {
  SOCKSEnable : 1
  SOCKSPort : 7892
  SOCKSProxy : 127.0.0.1
}"#;

        let config = parse_scutil_proxy(input).expect("proxy config");

        assert_eq!(
            config.all_proxy.as_deref(),
            Some("socks5h://127.0.0.1:7892")
        );
    }

    #[test]
    fn parse_windows_proxy_settings_extracts_per_scheme_proxy() {
        let config = parse_windows_proxy_settings(
            true,
            "http=127.0.0.1:7890;https=127.0.0.1:7891",
            Some("localhost;127.*;10.*;*.local;<local>"),
        )
        .expect("proxy config");

        assert_eq!(config.http_proxy.as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(config.https_proxy.as_deref(), Some("http://127.0.0.1:7891"));
        assert_eq!(config.all_proxy, None);

        let no_proxy = config.no_proxy.expect("no_proxy");
        assert!(no_proxy.contains("localhost"));
        assert!(no_proxy.contains("127.0.0.0/8"));
        assert!(no_proxy.contains("10.0.0.0/8"));
        assert!(no_proxy.contains(".local"));
    }

    #[test]
    fn parse_windows_proxy_settings_reuses_default_proxy_for_http_and_https() {
        let config =
            parse_windows_proxy_settings(true, "127.0.0.1:7890", None).expect("proxy config");

        assert_eq!(config.http_proxy.as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(config.https_proxy.as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(config.all_proxy, None);
    }

    #[test]
    fn parse_windows_proxy_settings_uses_socks_as_all_proxy_when_needed() {
        let config =
            parse_windows_proxy_settings(true, "socks=127.0.0.1:1080", None).expect("proxy config");

        assert_eq!(config.http_proxy, None);
        assert_eq!(config.https_proxy, None);
        assert_eq!(
            config.all_proxy.as_deref(),
            Some("socks5h://127.0.0.1:1080")
        );
    }

    #[test]
    fn parse_windows_proxy_settings_ignores_disabled_or_empty_proxy() {
        assert_eq!(
            parse_windows_proxy_settings(false, "127.0.0.1:7890", None),
            None
        );
        assert_eq!(parse_windows_proxy_settings(true, "   ", None), None);
    }

    #[test]
    fn parse_linux_gsettings_proxy_extracts_manual_proxy() {
        let config = parse_linux_gsettings_proxy(LinuxGSettingsProxy {
            mode: "'manual'".to_owned(),
            http_host: Some("'127.0.0.1'".to_owned()),
            http_port: Some("uint32 7890".to_owned()),
            https_host: Some("'127.0.0.1'".to_owned()),
            https_port: Some("7891".to_owned()),
            socks_host: Some("''".to_owned()),
            socks_port: Some("0".to_owned()),
            ignore_hosts: Some("['localhost', '127.0.0.0/8', '*.local']".to_owned()),
        })
        .expect("proxy config");

        assert_eq!(config.http_proxy.as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(config.https_proxy.as_deref(), Some("http://127.0.0.1:7891"));
        assert_eq!(config.all_proxy, None);

        let no_proxy = config.no_proxy.expect("no_proxy");
        assert!(no_proxy.contains("localhost"));
        assert!(no_proxy.contains("127.0.0.0/8"));
        assert!(no_proxy.contains(".local"));
    }

    #[test]
    fn parse_linux_gsettings_proxy_uses_socks_when_http_is_absent() {
        let config = parse_linux_gsettings_proxy(LinuxGSettingsProxy {
            mode: "'manual'".to_owned(),
            http_host: None,
            http_port: None,
            https_host: None,
            https_port: None,
            socks_host: Some("'127.0.0.1'".to_owned()),
            socks_port: Some("uint32 1080".to_owned()),
            ignore_hosts: Some("@as []".to_owned()),
        })
        .expect("proxy config");

        assert_eq!(config.http_proxy, None);
        assert_eq!(config.https_proxy, None);
        assert_eq!(
            config.all_proxy.as_deref(),
            Some("socks5h://127.0.0.1:1080")
        );
    }

    #[test]
    fn parse_linux_gsettings_proxy_ignores_non_manual_or_empty_proxy() {
        assert_eq!(
            parse_linux_gsettings_proxy(LinuxGSettingsProxy {
                mode: "'none'".to_owned(),
                http_host: Some("'127.0.0.1'".to_owned()),
                http_port: Some("7890".to_owned()),
                https_host: None,
                https_port: None,
                socks_host: None,
                socks_port: None,
                ignore_hosts: None,
            }),
            None
        );

        assert_eq!(
            parse_linux_gsettings_proxy(LinuxGSettingsProxy {
                mode: "'manual'".to_owned(),
                http_host: Some("''".to_owned()),
                http_port: Some("0".to_owned()),
                https_host: None,
                https_port: None,
                socks_host: None,
                socks_port: None,
                ignore_hosts: None,
            }),
            None
        );
    }

    #[test]
    fn parse_linux_kde_proxy_extracts_manual_proxy() {
        let config = parse_linux_kde_proxy(LinuxKdeProxy {
            proxy_type: "1".to_owned(),
            http_proxy: Some("http://127.0.0.1 7890".to_owned()),
            https_proxy: Some("http://127.0.0.1 7891".to_owned()),
            socks_proxy: Some("socks://127.0.0.1 1080".to_owned()),
            no_proxy: Some("localhost,127.*,*.local".to_owned()),
        })
        .expect("proxy config");

        assert_eq!(config.http_proxy.as_deref(), Some("http://127.0.0.1:7890"));
        assert_eq!(config.https_proxy.as_deref(), Some("http://127.0.0.1:7891"));
        assert_eq!(config.all_proxy, None);

        let no_proxy = config.no_proxy.expect("no_proxy");
        assert!(no_proxy.contains("localhost"));
        assert!(no_proxy.contains("127.0.0.0/8"));
        assert!(no_proxy.contains(".local"));
    }

    #[test]
    fn parse_linux_kde_proxy_uses_socks_when_http_is_absent() {
        let config = parse_linux_kde_proxy(LinuxKdeProxy {
            proxy_type: "1".to_owned(),
            http_proxy: None,
            https_proxy: None,
            socks_proxy: Some("socks://127.0.0.1 1080".to_owned()),
            no_proxy: None,
        })
        .expect("proxy config");

        assert_eq!(config.http_proxy, None);
        assert_eq!(config.https_proxy, None);
        assert_eq!(
            config.all_proxy.as_deref(),
            Some("socks5h://127.0.0.1:1080")
        );
    }

    #[test]
    fn parse_linux_kde_proxy_ignores_disabled_or_empty_proxy() {
        assert_eq!(
            parse_linux_kde_proxy(LinuxKdeProxy {
                proxy_type: "0".to_owned(),
                http_proxy: Some("http://127.0.0.1 7890".to_owned()),
                https_proxy: None,
                socks_proxy: None,
                no_proxy: None,
            }),
            None
        );

        assert_eq!(
            parse_linux_kde_proxy(LinuxKdeProxy {
                proxy_type: "1".to_owned(),
                http_proxy: Some("".to_owned()),
                https_proxy: None,
                socks_proxy: None,
                no_proxy: None,
            }),
            None
        );
    }
}
