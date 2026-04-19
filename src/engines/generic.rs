use crate::models::RawSearchResult;
use reqwest::Client;
use scraper::{Html, Selector};

#[derive(Clone, Copy)]
pub struct GenericEngineSpec {
    pub endpoint_template: &'static str,
    pub pages: usize,
}

pub struct GenericEngine {
    name: String,
    spec: GenericEngineSpec,
}

impl GenericEngine {
    pub fn new(name: &str, spec: GenericEngineSpec) -> Self {
        Self {
            name: name.to_string(),
            spec,
        }
    }
}

#[async_trait::async_trait]
impl super::SearchEngine for GenericEngine {
    fn name(&self) -> &str {
        &self.name
    }

    async fn search(&self, client: &Client, query: &str) -> Vec<RawSearchResult> {
        let mut results = Vec::new();
        let mut seen = std::collections::HashSet::new();

        let encoded = urlencoding::encode(query).to_string();

        for page_idx in 0..self.spec.pages {
            let offset = page_idx * 10;
            let page = page_idx + 1;
            let url = self
                .spec
                .endpoint_template
                .replace("{query}", &encoded)
                .replace("{offset}", &offset.to_string())
                .replace("{page}", &page.to_string());

            let req = crate::config::apply_browser_headers(client.get(&url), &url)
                .header("Accept", "text/html,application/xhtml+xml");

            let resp = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    tracing::debug!("{} request failed for {}: {}", self.name, url, e);
                    continue;
                }
            };

            let body = match resp.text().await {
                Ok(t) => t,
                Err(_) => continue,
            };

            let page_results = parse_generic_html(&body, &self.name, &url);
            if page_results.is_empty() {
                continue;
            }

            for r in page_results {
                if seen.insert(r.url.clone()) {
                    results.push(r);
                }
            }
        }

        results
    }
}

fn parse_generic_html(html: &str, engine_name: &str, engine_url: &str) -> Vec<RawSearchResult> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let host = url::Url::parse(engine_url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_string()))
        .unwrap_or_default();

    let selectors = [
        "#search a[href]",
        "article a[href]",
        "main a[href]",
        "h2 a[href]",
        "h3 a[href]",
        "li a[href]",
    ];

    let mut links = Vec::new();
    for sel in selectors {
        if let Ok(s) = Selector::parse(sel) {
            links.extend(doc.select(&s));
        }
        if links.len() > 120 {
            break;
        }
    }

    for link in links {
        let href = match link.value().attr("href") {
            Some(h) => h,
            None => continue,
        };

        let resolved = match resolve_target_url(href) {
            Some(u) => u,
            None => continue,
        };

        if !resolved.starts_with("http://") && !resolved.starts_with("https://") {
            continue;
        }
        if !host.is_empty() && resolved.contains(&host) {
            continue;
        }
        if !seen.insert(resolved.clone()) {
            continue;
        }

        let title = link.text().collect::<String>().trim().to_string();
        if title.len() < 3 {
            continue;
        }

        let snippet = link
            .value()
            .attr("aria-label")
            .map(|s| s.to_string())
            .unwrap_or_default();

        out.push(RawSearchResult {
            url: resolved,
            title,
            snippet,
            engine: engine_name.to_string(),
        });

        if out.len() >= 40 {
            break;
        }
    }

    out
}

fn resolve_target_url(href: &str) -> Option<String> {
    if href.starts_with("http://") || href.starts_with("https://") {
        return Some(href.to_string());
    }

    if href.starts_with("//") {
        return Some(format!("https:{}", href));
    }

    // Google-style redirect: /url?q=<target>
    if href.starts_with("/url?") {
        let fake = format!("https://www.google.com{}", href);
        if let Ok(parsed) = url::Url::parse(&fake) {
            for (k, v) in parsed.query_pairs() {
                if k == "q" || k == "url" {
                    return Some(v.to_string());
                }
            }
        }
    }

    // DuckDuckGo-style redirect with uddg.
    if href.contains("uddg=") {
        let fake = if href.starts_with("http") {
            href.to_string()
        } else {
            format!("https://duckduckgo.com{}", href)
        };
        if let Ok(parsed) = url::Url::parse(&fake) {
            for (k, v) in parsed.query_pairs() {
                if k == "uddg" {
                    if let Ok(decoded) = urlencoding::decode(&v) {
                        return Some(decoded.to_string());
                    }
                    return Some(v.to_string());
                }
            }
        }
    }

    None
}

pub fn spec_for(name: &str) -> Option<GenericEngineSpec> {
    let n = name.to_lowercase();

    if n.starts_with("google") {
        let domain = google_domain_for_engine(&n);

        if n.contains("_news") {
            return Some(GenericEngineSpec {
                endpoint_template: Box::leak(
                    format!("https://{}/search?q={{query}}&tbm=nws&start={{offset}}", domain)
                        .into_boxed_str(),
                ),
                pages: 3,
            });
        }

        if n.contains("_images") {
            return Some(GenericEngineSpec {
                endpoint_template: Box::leak(
                    format!("https://{}/search?q={{query}}&tbm=isch&start={{offset}}", domain)
                        .into_boxed_str(),
                ),
                pages: 2,
            });
        }

        if n.contains("_videos") {
            return Some(GenericEngineSpec {
                endpoint_template: Box::leak(
                    format!("https://{}/search?q={{query}}&tbm=vid&start={{offset}}", domain)
                        .into_boxed_str(),
                ),
                pages: 2,
            });
        }

        if n.contains("_scholar") {
            return Some(GenericEngineSpec {
                endpoint_template: "https://scholar.google.com/scholar?q={query}&start={offset}",
                pages: 3,
            });
        }

        return Some(GenericEngineSpec {
            endpoint_template: Box::leak(
                format!("https://{}/search?q={{query}}&start={{offset}}", domain).into_boxed_str(),
            ),
            pages: 3,
        });
    }

    if n.starts_with("bing") {
        if n.contains("_news") {
            return Some(GenericEngineSpec {
                endpoint_template: "https://www.bing.com/news/search?q={query}&first={offset}",
                pages: 3,
            });
        }

        if n.contains("_images") {
            return Some(GenericEngineSpec {
                endpoint_template: "https://www.bing.com/images/search?q={query}&first={offset}",
                pages: 2,
            });
        }

        if n.contains("_videos") {
            return Some(GenericEngineSpec {
                endpoint_template: "https://www.bing.com/videos/search?q={query}&first={offset}",
                pages: 2,
            });
        }

        return Some(GenericEngineSpec {
            endpoint_template: "https://www.bing.com/search?q={query}&first={offset}",
            pages: 3,
        });
    }

    if n.starts_with("duckduckgo") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://html.duckduckgo.com/html/?q={query}&s={offset}",
            pages: 3,
        });
    }

    if n.starts_with("brave") {
        let endpoint = if n.contains("_news") {
            "https://search.brave.com/news?q={query}&offset={offset}"
        } else {
            "https://search.brave.com/search?q={query}&source=web&offset={offset}"
        };
        return Some(GenericEngineSpec {
            endpoint_template: endpoint,
            pages: 3,
        });
    }

    if n.starts_with("yahoo") {
        let endpoint = if n.contains("_news") {
            "https://news.search.yahoo.com/search?p={query}&b={offset}"
        } else {
            "https://search.yahoo.com/search?p={query}&b={offset}"
        };
        return Some(GenericEngineSpec {
            endpoint_template: endpoint,
            pages: 3,
        });
    }

    if n.starts_with("yandex") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://yandex.com/search/?text={query}&p={page}",
            pages: 2,
        });
    }

    if n.starts_with("baidu") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://www.baidu.com/s?wd={query}&pn={offset}",
            pages: 2,
        });
    }

    if n.starts_with("ecosia") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://www.ecosia.org/search?q={query}&p={page}",
            pages: 2,
        });
    }

    if n.starts_with("metager") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://metager.org/meta/meta.ger3?eingabe={query}",
            pages: 2,
        });
    }

    if n.starts_with("swisscows") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://swisscows.com/web?query={query}",
            pages: 2,
        });
    }

    if n.starts_with("ask") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://www.ask.com/web?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("aol") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://search.aol.com/aol/search?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("lycos") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://search.lycos.com/web/?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("dogpile") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://www.dogpile.com/serp?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("gibiru") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://gibiru.com/results.html?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("searchencrypt") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://www.searchencrypt.com/search?eq={query}",
            pages: 2,
        });
    }

    if n.starts_with("presearch") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://presearch.com/search?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("yep") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://yep.com/web?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("mwmbl") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://mwmbl.org/?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("sogou") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://www.sogou.com/web?query={query}&page={page}",
            pages: 2,
        });
    }

    if n.starts_with("naver") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://search.naver.com/search.naver?query={query}",
            pages: 2,
        });
    }

    if n.starts_with("daum") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://search.daum.net/search?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("seznam") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://search.seznam.cz/?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("rambler") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://nova.rambler.ru/search?query={query}",
            pages: 2,
        });
    }

    if n.starts_with("searchalot") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://www.searchalot.com/result?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("excite") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://results.excite.com/serp?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("webcrawler") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://www.webcrawler.com/serp?q={query}",
            pages: 2,
        });
    }

    if n == "info" {
        return Some(GenericEngineSpec {
            endpoint_template: "https://www.info.com/serp?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("pipilika") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://www.pipilika.com/search?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("kiddle") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://www.kiddle.co/s.php?q={query}",
            pages: 2,
        });
    }

    // ── New engines added in v4.1 ──

    if n.starts_with("marginalia") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://search.marginalia.nu/search?query={query}",
            pages: 2,
        });
    }

    if n.starts_with("right_dao") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://rightdao.com/search?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("stract") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://stract.com/search?q={query}",
            pages: 2,
        });
    }

    // ── 2026: New engines for maximum coverage ──

    if n.starts_with("alexandria") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://alexandria.org/?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("4get") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://4get.ca/web?s={query}",
            pages: 2,
        });
    }

    if n.starts_with("whoogle") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://whoogle.io/search?q={query}",
            pages: 2,
        });
    }

    if n.starts_with("librex") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://librex.devol.it/search.php?q={query}&type=text",
            pages: 2,
        });
    }

    if n.starts_with("yacy") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://yacy.searchlab.eu/yacysearch.html?query={query}",
            pages: 2,
        });
    }

    if n.starts_with("mullvad_leta") {
        return Some(GenericEngineSpec {
            endpoint_template: "https://leta.mullvad.net/?q={query}",
            pages: 2,
        });
    }

    None
}

fn google_domain_for_engine(name: &str) -> &'static str {
    if name.ends_with("_uk") {
        "www.google.co.uk"
    } else if name.ends_with("_in") {
        "www.google.co.in"
    } else if name.ends_with("_de") {
        "www.google.de"
    } else if name.ends_with("_fr") {
        "www.google.fr"
    } else if name.ends_with("_es") {
        "www.google.es"
    } else if name.ends_with("_it") {
        "www.google.it"
    } else if name.ends_with("_br") {
        "www.google.com.br"
    } else if name.ends_with("_jp") {
        "www.google.co.jp"
    } else if name.ends_with("_ca") {
        "www.google.ca"
    } else if name.ends_with("_au") {
        "www.google.com.au"
    } else if name.ends_with("_nl") {
        "www.google.nl"
    } else if name.ends_with("_se") {
        "www.google.se"
    } else if name.ends_with("_no") {
        "www.google.no"
    } else if name.ends_with("_fi") {
        "www.google.fi"
    } else {
        "www.google.com"
    }
}
