// ============================================================================
// SearchWala v8.0 — SSRF guard for caller-supplied URLs
//
// WHY THIS EXISTS
// `LlmConfig.base_url` is attacker-controllable: it arrives in the request
// body and `resolve_provider` used it verbatim. Combined with
// `CorsLayer::permissive()` and a 0.0.0.0 bind, v7.0 was an open, credentialed
// SSRF proxy — any page the user had open could do:
//
//   fetch('http://localhost:8000/api/models', {
//     method: 'POST',
//     body: JSON.stringify({ api_key: 'x',
//                            base_url: 'http://169.254.169.254/latest/meta-data/' })
//   })
//
// and read the response. Cloud metadata endpoints, loopback admin panels and
// private-range services were all reachable.
//
// Local models are a LEGITIMATE reason to point at loopback (Ollama,
// LM Studio, llama.cpp), so this is not a blanket ban: loopback and private
// ranges are permitted only when the operator opts in with
// ALLOW_LOCAL_LLM=1. Cloud metadata addresses are never permitted.
// ============================================================================

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

use url::{Host, Url};

/// Hostnames that are never acceptable, opt-in or not.
const ALWAYS_BLOCKED_HOSTS: &[&str] = &[
    "metadata.google.internal",
    "metadata.goog",
    "instance-data",
];

/// Cloud metadata IPs — never reachable regardless of opt-in.
const ALWAYS_BLOCKED_IPS: &[Ipv4Addr] = &[
    Ipv4Addr::new(169, 254, 169, 254), // AWS / GCP / Azure / DO IMDS
    Ipv4Addr::new(169, 254, 170, 2),   // AWS ECS task metadata
    Ipv4Addr::new(100, 100, 100, 200), // Alibaba Cloud
];

fn allow_local_from_env() -> bool {
    matches!(
        std::env::var("ALLOW_LOCAL_LLM").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn is_private_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.is_documentation()
        // 100.64.0.0/10 carrier-grade NAT
        || (ip.octets()[0] == 100 && (64..128).contains(&ip.octets()[1]))
        // 0.0.0.0/8
        || ip.octets()[0] == 0
}

fn is_private_v6(ip: Ipv6Addr) -> bool {
    ip.is_loopback()
        || ip.is_unspecified()
        // fc00::/7 unique local
        || (ip.segments()[0] & 0xfe00) == 0xfc00
        // fe80::/10 link local
        || (ip.segments()[0] & 0xffc0) == 0xfe80
        // IPv4-mapped — check the embedded address
        || ip.to_ipv4_mapped().map(is_private_v4).unwrap_or(false)
}

/// Validate a caller-supplied LLM base URL, reading the opt-in from the
/// environment.
pub fn validate_llm_base_url(raw: &str) -> Result<Url, String> {
    validate_base_url_with(raw, allow_local_from_env())
}

/// Testable core. `allow_local` is a parameter rather than an env read because
/// `std::env::set_var` is process-global and `cargo test` runs in parallel —
/// env-var-driven tests race against each other (this one did).
pub fn validate_base_url_with(raw: &str, allow_local: bool) -> Result<Url, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("base_url is empty".into());
    }
    let u = Url::parse(trimmed).map_err(|e| format!("base_url is not a valid URL: {e}"))?;

    match u.scheme() {
        "https" => {}
        "http" if allow_local => {}
        "http" => {
            return Err(
                "base_url must use https (set ALLOW_LOCAL_LLM=1 to allow http for a local model)"
                    .into(),
            )
        }
        other => return Err(format!("base_url scheme {other:?} is not allowed")),
    }

    let host = u.host().ok_or_else(|| "base_url has no host".to_string())?;

    match host {
        Host::Domain(d) => {
            let dl = d.to_lowercase();
            if ALWAYS_BLOCKED_HOSTS.iter().any(|b| dl == *b || dl.ends_with(&format!(".{b}"))) {
                return Err(format!("base_url host {d:?} is blocked"));
            }
            if (dl == "localhost" || dl.ends_with(".localhost") || dl.ends_with(".local"))
                && !allow_local
            {
                return Err(
                    "base_url points at localhost (set ALLOW_LOCAL_LLM=1 to allow a local model)"
                        .into(),
                );
            }
        }
        Host::Ipv4(ip) => {
            if ALWAYS_BLOCKED_IPS.contains(&ip) {
                return Err(format!("base_url {ip} is a cloud metadata endpoint"));
            }
            if is_private_v4(ip) && !allow_local {
                return Err(format!(
                    "base_url {ip} is a private address (set ALLOW_LOCAL_LLM=1 to allow it)"
                ));
            }
        }
        Host::Ipv6(ip) => {
            if is_private_v6(ip) && !allow_local {
                return Err(format!(
                    "base_url {ip} is a private address (set ALLOW_LOCAL_LLM=1 to allow it)"
                ));
            }
        }
    }

    Ok(u)
}

/// Is this resolved IP safe to connect to?
///
/// Guards against DNS rebinding: a public hostname can resolve to 127.0.0.1.
pub fn is_ip_allowed(ip: IpAddr) -> bool {
    is_ip_allowed_with(ip, allow_local_from_env())
}

pub fn is_ip_allowed_with(ip: IpAddr, allow_local: bool) -> bool {
    match ip {
        IpAddr::V4(v4) => !ALWAYS_BLOCKED_IPS.contains(&v4) && (!is_private_v4(v4) || allow_local),
        IpAddr::V6(v6) => !is_private_v6(v6) || allow_local,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These call the parameterised core, so they do not touch process-global
    // env vars and cannot race under the parallel test harness.
    const DENY: bool = false;
    const ALLOW: bool = true;

    #[test]
    fn blocks_cloud_metadata_endpoints_even_with_opt_in() {
        for allow in [DENY, ALLOW] {
            for u in [
                "http://169.254.169.254/latest/meta-data/",
                "https://169.254.169.254/",
                "http://169.254.170.2/v2/credentials",
                "http://100.100.100.200/latest/meta-data/",
                "http://metadata.google.internal/computeMetadata/v1/",
            ] {
                assert!(
                    validate_base_url_with(u, allow).is_err(),
                    "metadata endpoint must be blocked unconditionally: {u} (allow_local={allow})"
                );
            }
        }
    }

    #[test]
    fn blocks_loopback_and_private_ranges_by_default() {
        for u in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:8080/v1",
            "https://127.0.0.1/v1",
            "http://10.0.0.5/v1",
            "http://192.168.1.1/v1",
            "http://172.16.0.1/v1",
            "http://0.0.0.0/v1",
            "http://[::1]/v1",
            "http://[fc00::1]/v1",
            "https://foo.local/v1",
        ] {
            assert!(
                validate_base_url_with(u, DENY).is_err(),
                "should be blocked by default: {u}"
            );
        }
    }

    #[test]
    fn permits_local_models_when_opted_in() {
        // Ollama / LM Studio / llama.cpp are the legitimate case.
        for u in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:1234/v1",
            "http://192.168.1.50:8000/v1",
            "http://[::1]:8080/v1",
        ] {
            assert!(
                validate_base_url_with(u, ALLOW).is_ok(),
                "should be allowed with opt-in: {u}"
            );
        }
    }

    #[test]
    fn requires_https_for_public_endpoints() {
        assert!(validate_base_url_with("http://api.openai.com/v1", DENY).is_err());
        assert!(validate_base_url_with("https://api.openai.com/v1", DENY).is_ok());
    }

    #[test]
    fn rejects_non_http_schemes() {
        for u in ["file:///etc/passwd", "gopher://x/", "ftp://x/", "javascript:alert(1)"] {
            for allow in [DENY, ALLOW] {
                assert!(validate_base_url_with(u, allow).is_err(), "{u}");
            }
        }
    }

    #[test]
    fn allows_normal_provider_endpoints() {
        for u in [
            "https://api.openai.com/v1",
            "https://api.anthropic.com/",
            "https://generativelanguage.googleapis.com/v1beta/openai/",
            "https://api.groq.com/openai/v1",
            "https://openrouter.ai/api/v1",
            "https://api.deepseek.com/v1",
        ] {
            assert!(validate_base_url_with(u, DENY).is_ok(), "{u} should be allowed");
        }
    }

    #[test]
    fn ip_allowlist_guards_dns_rebinding() {
        // A public hostname can resolve to 127.0.0.1; this is the second line
        // of defence.
        assert!(!is_ip_allowed_with("127.0.0.1".parse().unwrap(), DENY));
        assert!(!is_ip_allowed_with("169.254.169.254".parse().unwrap(), ALLOW));
        assert!(!is_ip_allowed_with("10.1.2.3".parse().unwrap(), DENY));
        assert!(!is_ip_allowed_with("::1".parse().unwrap(), DENY));
        assert!(is_ip_allowed_with("1.1.1.1".parse().unwrap(), DENY));
        assert!(is_ip_allowed_with("2606:4700::1111".parse().unwrap(), DENY));
        assert!(is_ip_allowed_with("127.0.0.1".parse().unwrap(), ALLOW));
    }

    #[test]
    fn rejects_garbage_input() {
        for u in ["", "   ", "not a url", "https://"] {
            assert!(validate_base_url_with(u, ALLOW).is_err(), "{u:?}");
        }
    }

    #[test]
    fn carrier_grade_nat_and_zero_network_are_private() {
        assert!(validate_base_url_with("http://100.64.0.1/v1", DENY).is_err());
        assert!(validate_base_url_with("http://0.1.2.3/v1", DENY).is_err());
    }

    #[test]
    fn ipv4_mapped_ipv6_loopback_is_caught() {
        // ::ffff:127.0.0.1 must not slip past the v6 branch.
        assert!(validate_base_url_with("http://[::ffff:127.0.0.1]/v1", DENY).is_err());
    }
}
