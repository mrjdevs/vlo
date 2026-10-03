use std::{collections::HashMap, sync::LazyLock};
use chrono::Datelike;
use serde_json::Value;

pub type ModifierFn = fn(String, Option<String>) -> String;

// ── Text ──────────────────────────────────────────────────────────────
fn m_upper(v: String, _: Option<String>) -> String { v.to_uppercase() }
fn m_lower(v: String, _: Option<String>) -> String { v.to_lowercase() }
fn m_proper(v: String, _: Option<String>) -> String {
    v.split_whitespace().map(|w| {
        let mut c = w.chars();
        match c.next() { None => String::new(), Some(f) => f.to_uppercase().collect::<String>() + &c.as_str().to_lowercase() }
    }).collect::<Vec<_>>().join(" ")
}
fn m_capitalize(v: String, _: Option<String>) -> String {
    let mut c = v.chars(); match c.next() { None => String::new(), Some(f) => f.to_uppercase().collect::<String>() + c.as_str() }
}
fn m_lcfirst(v: String, _: Option<String>) -> String {
    let mut c = v.chars(); match c.next() { None => String::new(), Some(f) => f.to_lowercase().collect::<String>() + c.as_str() }
}
fn m_trim(v: String, _: Option<String>) -> String { v.trim().to_string() }
fn m_ltrim(v: String, _: Option<String>) -> String { v.trim_start().to_string() }
fn m_rtrim(v: String, _: Option<String>) -> String { v.trim_end().to_string() }
fn m_slug(v: String, _: Option<String>) -> String {
    v.to_lowercase().split_whitespace().collect::<Vec<_>>().join("-")
    .chars().filter(|c| c.is_alphanumeric() || *c == '-').collect()
}
fn m_reverse(v: String, _: Option<String>) -> String { v.chars().rev().collect() }
fn m_truncate(v: String, arg: Option<String>) -> String {
    let a = arg.unwrap_or_else(|| "50".into());
    let parts: Vec<&str> = a.splitn(2, ':').collect();
    let lim: usize = parts[0].parse().unwrap_or(50);
    let suf = parts.get(1).unwrap_or(&"...").to_string();
    if v.chars().count() > lim { format!("{}{}", v.chars().take(lim).collect::<String>(), suf) } else { v }
}
fn m_truncate_words(v: String, arg: Option<String>) -> String {
    let lim: usize = arg.as_deref().unwrap_or("10").parse().unwrap_or(10);
    let words: Vec<&str> = v.split_whitespace().collect();
    if words.len() > lim { format!("{}...", words[..lim].join(" ")) } else { v }
}
fn m_replace(v: String, arg: Option<String>) -> String {
    let Some(arg) = arg else { return v };
    let mut p = arg.splitn(2, ',');
    v.replace(p.next().unwrap_or("").trim(), p.next().unwrap_or("").trim())
}
fn m_pad(v: String, arg: Option<String>) -> String {
    let len: usize = arg.as_deref().unwrap_or("10").parse().unwrap_or(10);
    if v.len() >= len { v } else { format!("{}{}", " ".repeat(len - v.len()), v) }
}
fn m_words(v: String, _: Option<String>) -> String { v.split_whitespace().count().to_string() }
fn m_len(v: String, _: Option<String>) -> String { v.chars().count().to_string() }
fn m_string(v: String, _: Option<String>) -> String { v }
fn m_escape(v: String, _: Option<String>) -> String {
    v.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}
fn m_nl2br(v: String, _: Option<String>) -> String { v.replace('\n', "<br>") }
fn m_upper_words(v: String, arg: Option<String>) -> String {
    let n: usize = arg.as_deref().unwrap_or("1").parse().unwrap_or(1);
    v.split_whitespace().enumerate().map(|(i,w)| if i<n { w.to_uppercase() } else { w.to_string() }).collect::<Vec<_>>().join(" ")
}

// ── Number ────────────────────────────────────────────────────────────
fn m_number(v: String, arg: Option<String>) -> String {
    let dec: usize = arg.as_deref().unwrap_or("0").parse().unwrap_or(0);
    let n: f64 = v.replace(",", "").parse().unwrap_or(0.0);
    let fmt = format!("{:.*}", dec, n);
    let parts: Vec<&str> = fmt.split('.').collect();
    let intp = parts[0].chars().rev().collect::<Vec<_>>().chunks(3).map(|c| c.iter().collect::<String>()).collect::<Vec<_>>().join(",").chars().rev().collect::<String>();
    if parts.len()>1 { format!("{}.{}", intp, parts[1]) } else { intp }
}
fn m_double(v: String, arg: Option<String>) -> String {
    let d: usize = arg.as_deref().unwrap_or("2").parse().unwrap_or(2);
    let n: f64 = v.parse().unwrap_or(0.0); format!("{:.*}", d, n)
}
fn m_int(v: String, _: Option<String>) -> String { let n: f64 = v.parse().unwrap_or(0.0); format!("{}", n as i64) }
fn m_currency(v: String, arg: Option<String>) -> String {
    let sym = arg.unwrap_or_else(|| "$".into()); format!("{} {}", sym, m_number(v, Some("2".into())))
}
fn m_percent(v: String, arg: Option<String>) -> String {
    let d: usize = arg.as_deref().unwrap_or("0").parse().unwrap_or(0);
    let n: f64 = v.parse().unwrap_or(0.0)*100.0; format!("{:.*}%", d, n)
}

// ── Number / Date / Time ─────────────────────────────────────────────
fn m_format(v: String, arg: Option<String>) -> String {
    let Some(fmt) = arg else { return v };
    let fmt = fmt.trim();
    if let Some(dt) = parse_datetime(&v) {
        match fmt.to_lowercase().as_str() {
            "year" | "yr" => return dt.format("%Y").to_string(),
            "month" | "mon" => return dt.format("%Y-%m").to_string(),
            "month_name" | "monthname" => return dt.format("%B").to_string(),
            "month_short" | "mon_short" => return dt.format("%b").to_string(),
            "quarter" | "qtr" | "q" => return format!("{}-Q{}", dt.year(), (dt.month() - 1) / 3 + 1),
            "week" | "wk" | "week_number" => {
                let w = dt.iso_week();
                return format!("{}-W{:02}", w.year(), w.week());
            }
            "day" | "date" | "date_only" => return dt.format("%Y-%m-%d").to_string(),
            "day_name" | "weekday" => return dt.format("%A").to_string(),
            "day_short" | "weekday_short" => return dt.format("%a").to_string(),
            "hour" => return dt.format("%H").to_string(),
            "minute" | "min" => return dt.format("%H:%M").to_string(),
            "time" => return dt.format("%H:%M:%S").to_string(),
            "datetime" => return dt.format("%Y-%m-%d %H:%M:%S").to_string(),
            _ => {}
        }
        if fmt.contains('%') { return dt.format(fmt).to_string(); }
    }
    let n = v.trim().parse::<f64>().unwrap_or(0.0);
    fmt.replace("%.2f", &format!("{:.2}", n))
        .replace("%.1f", &format!("{:.1}", n))
        .replace("%f", &format!("{:.2}", n))
        .replace("%s", &v)
}

fn m_year(v: String, _: Option<String>) -> String { m_format(v, Some("year".into())) }
fn m_month(v: String, _: Option<String>) -> String { m_format(v, Some("month".into())) }
fn m_month_name(v: String, _: Option<String>) -> String { m_format(v, Some("month_name".into())) }
fn m_month_short(v: String, _: Option<String>) -> String { m_format(v, Some("month_short".into())) }
fn m_quarter(v: String, _: Option<String>) -> String { m_format(v, Some("quarter".into())) }
fn m_week(v: String, _: Option<String>) -> String { m_format(v, Some("week".into())) }
fn m_day(v: String, _: Option<String>) -> String { m_format(v, Some("day".into())) }
fn m_day_name(v: String, _: Option<String>) -> String { m_format(v, Some("day_name".into())) }
fn m_day_short(v: String, _: Option<String>) -> String { m_format(v, Some("day_short".into())) }
fn m_hour(v: String, _: Option<String>) -> String { m_format(v, Some("hour".into())) }
fn m_minute(v: String, _: Option<String>) -> String { m_format(v, Some("minute".into())) }
fn m_time(v: String, _: Option<String>) -> String { m_format(v, Some("time".into())) }
fn m_datetime(v: String, _: Option<String>) -> String { m_format(v, Some("datetime".into())) }
fn m_date(v: String, arg: Option<String>) -> String {
    m_format(v, arg.or_else(|| Some("day".into())))
}
fn m_timeago(v: String, _: Option<String>) -> String {
    let Some(dt) = parse_datetime(&v) else {
        if let Ok(ts) = v.trim().parse::<i64>() {
            return timeago(chrono::Utc::now().timestamp() - ts);
        }
        return v;
    };
    timeago(chrono::Utc::now().timestamp() - dt.and_utc().timestamp())
}
fn timeago(diff: i64) -> String {
    if diff <= 0 { return "just now".into(); }
    match diff {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", diff / 60),
        3600..=86399 => format!("{}h ago", diff / 3600),
        _ => format!("{}d ago", diff / 86400),
    }
}
fn parse_datetime(v: &str) -> Option<chrono::NaiveDateTime> {
    let v = v.trim().trim_matches('"').trim_matches('\'');
    for f in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S", "%Y-%m-%dT%H:%M"] {
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(v, f) { return Some(dt); }
    }
    chrono::NaiveDate::parse_from_str(v, "%Y-%m-%d").ok()?.and_hms_opt(0, 0, 0)
}

// ── Type / Logic ─────────────────────────────────────────────────────
fn value_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "number",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}
fn value_is(v: &Value, check: &str) -> bool {
    match check.to_lowercase().as_str() {
        "null" => v.is_null(),
        "empty" => match v {
            Value::Null => true,
            Value::String(s) => s.trim().is_empty(),
            Value::Array(a) => a.is_empty(),
            Value::Object(o) => o.is_empty(),
            _ => false,
        },
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.as_i64().is_some() || v.as_u64().is_some(),
        "boolean" | "bool" => v.is_boolean(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        "email" => v.as_str().map(is_email).unwrap_or(false),
        "positive" => v.as_f64().map(|n| n > 0.0).unwrap_or(false),
        "negative" => v.as_f64().map(|n| n < 0.0).unwrap_or(false),
        _ => false,
    }
}
fn is_email(v: &str) -> bool {
    let mut p = v.split('@');
    matches!((p.next(), p.next(), p.next()), (Some(a), Some(b), None) if !a.is_empty() && b.contains('.') && !b.starts_with('.') && !b.ends_with('.'))
}
fn m_default(v: String, arg: Option<String>) -> String {
    if v.trim().is_empty() { arg.unwrap_or_default() } else { v }
}

// ── Security / Privacy ───────────────────────────────────────────────
fn m_mask(v: String, arg: Option<String>) -> String {
    match arg.as_deref().map(str::to_lowercase).as_deref() {
        Some("email") => {
            let mut p = v.split('@');
            let local = p.next().unwrap_or("");
            let domain = p.next().unwrap_or("");
            if local.is_empty() || domain.is_empty() { return "***".into(); }
            let first = local.chars().next().unwrap_or('*');
            let dfirst = domain.chars().next().unwrap_or('*');
            let suffix = domain.rsplit_once('.').map(|(_, x)| format!(".{}", x)).unwrap_or_default();
            format!("{}***@{}***{}", first, dfirst, suffix)
        }
        Some("ssn") => {
            let digits: String = v.chars().filter(|c| c.is_ascii_digit()).collect();
            if digits.len() == 9 { format!("***-**-{}", &digits[5..]) } else { "***".into() }
        }
        _ => {
            let c: Vec<char> = v.chars().collect();
            if c.len() <= 4 { "***".into() }
            else { format!("{}{}{}", c[..2].iter().collect::<String>(), "*".repeat(c.len() - 4), c[c.len() - 2..].iter().collect::<String>()) }
        }
    }
}
fn m_hash(v: String, arg: Option<String>) -> String {
    use sha2::{Digest, Sha256};
    match arg.unwrap_or_else(|| "sha256".into()).to_lowercase().as_str() {
        "sha256" | "sha-256" | "sha" => {
            let mut h = Sha256::new();
            h.update(v.as_bytes());
            format!("{:x}", h.finalize())
        }
        _ => v,
    }
}

// ── JSON / URL ───────────────────────────────────────────────────────
fn m_json(v: String, _: Option<String>) -> String {
    serde_json::to_string(&v).unwrap_or_default()
}
fn m_json_path(v: String, arg: Option<String>) -> String {
    let Some(path) = arg else { return v };
    let Ok(json) = serde_json::from_str::<Value>(&v) else { return String::new() };
    json_path(&json, &path).map(value_to_string).unwrap_or_default()
}
fn json_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = value;
    let path = path.trim().strip_prefix('$').unwrap_or(path.trim()).trim_start_matches('.');
    if path.is_empty() { return Some(current); }
    for part in path.split('.') {
        if part.is_empty() { continue; }
        if let Some((key, index)) = part.strip_suffix(']').and_then(|p| p.rsplit_once('[')) {
            current = current.get(key)?;
            current = current.get(index.parse::<usize>().ok()?)?;
        } else if let Ok(index) = part.parse::<usize>() {
            current = current.get(index)?;
        } else {
            current = current.get(part)?;
        }
    }
    Some(current)
}
fn m_query(v: String, arg: Option<String>) -> String {
    let key = arg.unwrap_or_default();
    let query = v.split_once('?').map(|(_, q)| q.split('#').next().unwrap_or(q)).unwrap_or("");
    query.split('&')
        .filter_map(|p| p.split_once('='))
        .find(|(k, _)| urlencoding::decode(k).map(|x| x == key).unwrap_or(false))
        .and_then(|(_, val)| urlencoding::decode(val).ok())
        .map(|x| x.into_owned())
        .unwrap_or_default()
}

// ── HTML / Content ──────────────────────────────────────────────────
fn m_strip_tags(v: String, _: Option<String>) -> String {
    let mut out = String::with_capacity(v.len());
    let mut inside = false;
    for c in v.chars() {
        match c {
            '<' => inside = true,
            '>' => inside = false,
            _ if !inside => out.push(c),
            _ => {}
        }
    }
    out
}
fn m_excerpt(v: String, arg: Option<String>) -> String {
    let limit = arg.as_deref().unwrap_or("150").parse::<usize>().unwrap_or(150);
    let chars: Vec<char> = v.chars().collect();
    if chars.len() <= limit { return v; }
    let mut end = limit;
    while end > 0 && !chars[end].is_whitespace() { end -= 1; }
    if end == 0 { end = limit; }
    format!("{}...", chars[..end].iter().collect::<String>().trim_end())
}
// ── Image Optimization ──────────────────────────────────────────────────
fn m_image(v: String, arg: Option<String>) -> String {
    let url = v.trim().trim_matches('"').trim_matches('\'');
    if url.is_empty() { return String::new(); }

    let mut width = 800;
    let mut height = 600;
    let mut format = String::from("webp");
    let mut blur = false;
    let mut quality = 80;
    let mut sizes = String::from("100vw");

    if let Some(options) = arg {
        for opt in options.split(',') {
            let opt = opt.trim();

            if opt.starts_with("w=") {
                width = opt[2..].parse().unwrap_or(width);
            } else if opt.starts_with("h=") {
                height = opt[2..].parse().unwrap_or(height);
            } else if opt.starts_with("q=") {
                quality = opt[2..].parse().unwrap_or(quality);
            } else if opt.starts_with("sizes=") {
                sizes = opt[6..].to_string();
            } else if opt == "webp" || opt == "avif" || opt == "jpg" || opt == "png" {
                format = opt.to_string();
            } else if opt == "blur" {
                blur = true;
            }
        }
    }

    let encoded_url = urlencoding::encode(url);
    let opt_base = "/api/vlo/optimize";

    let src_1x = format!(
        "{}?src={}&w={}&h={}&f={}&q={}",
        opt_base, encoded_url, width, height, format, quality
    );

    let src_2x = format!(
        "{}?src={}&w={}&h={}&f={}&q={}",
        opt_base, encoded_url, width * 2, height * 2, format, quality
    );

    let mut picture = String::new();

    // 1. AVIF Source
    if format != "avif" {
        let avif_src = format!(
            "{}?src={}&w={}&h={}&f=avif&q={}",
            opt_base, encoded_url, width, height, quality
        );

        let avif_srcset = format!(
            "{} 1x, {}?src={}&w={}&h={}&f=avif&q={} 2x",
            avif_src, opt_base, encoded_url, width * 2, height * 2, quality
        );

        picture.push_str(&format!(
            r#"<source type="image/avif" srcset="{}" sizes="{}">"#,
            avif_srcset, sizes
        ));
    }

    // 2. WebP Source
    if format != "webp" {
        let webp_src = format!(
            "{}?src={}&w={}&h={}&f=webp&q={}",
            opt_base, encoded_url, width, height, quality
        );

        let webp_srcset = format!(
            "{} 1x, {}?src={}&w={}&h={}&f=webp&q={} 2x",
            webp_src, opt_base, encoded_url, width * 2, height * 2, quality
        );

        picture.push_str(&format!(
            r#"<source type="image/webp" srcset="{}" sizes="{}">"#,
            webp_srcset, sizes
        ));
    }

    // 3. Fallback Img
    let img_srcset = format!("{} 1x, {} 2x", src_1x, src_2x);

    picture.push_str(&format!(
        r#"<img src="{}" srcset="{}" width="{}" height="{}" sizes="{}" loading="lazy" decoding="async" alt="Optimized Image""#,
        src_1x, img_srcset, width, height, sizes
    ));

    // 4. Dependency-free inline SVG blur placeholder
    if blur {
        let blur_svg = format!(
            r#"data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 {} {}'%3E%3Cfilter id='b'%3E%3CfeGaussianBlur stdDeviation='10'/%3E%3C/filter%3E%3Cimage width='100%25' height='100%25' filter='url(%23b)' href='{}'/%3E%3C/svg%3E"#,
            width, height, encoded_url
        );

        picture.push_str(&format!(
            r#" style="background: url('{}') no-repeat center / cover; opacity: 0; transition: opacity 0.4s ease-in-out;" onload="this.style.opacity=1" onerror="this.style.opacity=1""#,
            blur_svg
        ));
    } else {
        picture.push_str(r#" style="opacity: 1;""#);
    }

    picture.push_str(" /></picture>");
    picture
}
// ── Real-time / Live Updates ─────────────────────────────────────────────
fn m_live(v: String, arg: Option<String>) -> String {
    let channel = arg.unwrap_or_else(|| "default".into());
    format!(
        r#"<span class="vlo-live" data-channel="{}">{}</span><script>
(function(){{
    if(!window.__VLO_SSE__) {{
        window.__VLO_SSE__ = new EventSource("/__vlo_sse");
        window.__VLO_SSE__.onmessage = function(e) {{
            try {{
                const data = JSON.parse(e.data);
                if(data.channel === "{}") {{
                    document.querySelectorAll(`[data-channel="{}"]`).forEach(el => {{
                        // 🔥 SMART PAYLOAD HANDLING
                        if (typeof data.value === 'object' && data.value !== null) {{
                            // If it's a full API response, trigger global AJAX wrapper refresh
                            window.dispatchEvent(new CustomEvent('vlo:mutation'));
                        }} else {{
                            // If it's a simple string/number, update the text directly
                            el.innerHTML = data.value;
                            el.classList.add("vlo-live-updated");
                            setTimeout(() => el.classList.remove("vlo-live-updated"), 600);
                        }}
                    }});
                }}
            }} catch(err) {{ console.error("VLO SSE Error:", err); }}
        }};
    }}
}})();
</script>"#,
        channel, v, channel, channel
    )
}


// ── AJAX Core Script (injected ONCE by wrap_html) ─────────────────────
// ── AJAX Core: Base queue processor (always needed if any |ajax used) ──
// ── AJAX Core: Base queue processor (always needed if any |ajax used) ──
pub const AJAX_CORE_BASE: &str = r##"
(function() {
  if (window.__VLO_AJAX_READY) return;
  window.__VLO_AJAX_READY = true;
  var V = window.__VLO_AJAX = { targets: [], seq: {}, busy: {}, io: null };
  var Q = window.__VLO_AJAX_Q || [];

  window.__VLO_BROADCAST__ = function(channel, value) {
    fetch('/api/broadcast', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      credentials: 'same-origin',
      body: JSON.stringify({ channel: channel, value: value })
    }).catch(function(e) { console.error('[VLO Broadcast]', e); });
  };

  function refresh(url, sel, push) {
    var el = document.querySelector(sel);
    if (!el) return;
    var id = V.seq[sel] = (V.seq[sel] || 0) + 1;
    el.style.opacity = "0.5";
    fetch(url, { credentials: "same-origin" })
      .then(function(r) { return r.text(); })
      .then(function(html) {
        if (id !== V.seq[sel]) return;
        var doc = new DOMParser().parseFromString(html, "text/html");
        var fresh = doc.querySelector(sel);
        if (!fresh) { location.href = url; return; }
        el.innerHTML = fresh.innerHTML;
        if (push) history.pushState({}, "", url);
        if (window.__VLO_AJAX_WATCH__) window.__VLO_AJAX_WATCH__(sel);
      })
      .catch(function(e) { console.error("[VLO AJAX]", e); })
      .then(function() { if (id === V.seq[sel]) el.style.opacity = ""; });
  }

  function register(sel, url, interval) {
    if (V.targets.indexOf(sel) === -1) V.targets.push(sel);
    if (url) refresh(url, sel, false);
    if (interval > 0) setInterval(function() { refresh(url || location.href, sel, false); }, interval);
    if (document.readyState === "loading") {
      document.addEventListener("DOMContentLoaded", function() { if (window.__VLO_AJAX_WATCH__) window.__VLO_AJAX_WATCH__(sel); });
    } else {
      if (window.__VLO_AJAX_WATCH__) window.__VLO_AJAX_WATCH__(sel);
    }
  }

  function owner(node) {
    for (var i = 0; i !== V.targets.length; i++) {
      var t = document.querySelector(V.targets[i]);
      if (t && t.contains(node)) return V.targets[i];
    }
    return null;
  }

  document.addEventListener("click", function(e) {
    if (e.defaultPrevented || e.button !== 0 || e.metaKey || e.ctrlKey || e.shiftKey || e.altKey) return;
    var more = e.target.closest ? e.target.closest("[data-load-more]") : null;
    if (more) {
      e.preventDefault();
      var ms = owner(more);
      if (ms && window.__VLO_AJAX_LOADMORE__) window.__VLO_AJAX_LOADMORE__(ms);
      return;
    }
    var a = e.target.closest ? e.target.closest("a[href]") : null;
    if (!a || a.target || a.hasAttribute("download") || a.classList.contains("no-ajax")) return;
    var href = a.getAttribute("href");
    if (!href || href.charAt(0) === "#" || href.indexOf("javascript:") === 0) return;
    var u = new URL(a.href, location.href);
    if (u.origin !== location.origin) return;
    var sel = owner(a);
    if (!sel) return;
    e.preventDefault();
    refresh(u.href, sel, true);
  });

  document.addEventListener("submit", function(e) {
    var f = e.target;
    if (e.defaultPrevented || f.classList.contains("no-ajax")) return;
    if ((f.getAttribute("method") || "get").toLowerCase() !== "get") return;
    var sel = owner(f);
    if (!sel) return;
    e.preventDefault();
    var u = new URL(f.getAttribute("action") || location.pathname, location.href);
    var q = new URLSearchParams();
    new FormData(f).forEach(function(v, k) {
      if (typeof v === "string" && v !== "") q.append(k, v);
    });
    u.search = q.toString();
    refresh(u.href, sel, true);
  });

  var _mutationTimer = null;
  window.addEventListener("vlo:mutation", function() {
    if (_mutationTimer) clearTimeout(_mutationTimer);
    _mutationTimer = setTimeout(function() {
      V.targets.forEach(function(s) { refresh(location.href, s, false); });
    }, 100);
  });

  window.addEventListener("popstate", function() {
    V.targets.forEach(function(s) { refresh(location.href, s, false); });
  });

  window.__VLO_AJAX_REFRESH__ = refresh;
  window.__VLO_AJAX_OWNER__ = owner;

  Q.forEach(function(cfg) { register(cfg.sel, cfg.url, cfg.interval); });
})();
"##;

// ── AJAX Core: Load-more / infinite scroll (only if data-load-more exists) ──
pub const AJAX_CORE_LOADMORE: &str = r##"
(function() {
  var V = window.__VLO_AJAX;
  if (!V || !V.targets.length) return;
  var refresh = window.__VLO_AJAX_REFRESH__;
  var owner = window.__VLO_AJAX_OWNER__;
  if (!refresh || !owner) return;

  function loadMore(sel) {
    var el = document.querySelector(sel);
    var btn = el ? el.querySelector("[data-load-more]") : null;
    if (!btn || V.busy[sel]) return;
    V.busy[sel] = true;
    var originalText = btn.innerText;
    btn.innerText = "Loading...";
    btn.style.opacity = "0.7";
    var next = new URL(btn.getAttribute("href") || "", location.href);
    var u = new URL(location.href);
    u.searchParams.set("page", next.searchParams.get("page") || "2");
    fetch(u.href, { credentials: "same-origin" })
      .then(function(r) { return r.text(); })
      .then(function(html) {
        var doc = new DOMParser().parseFromString(html, "text/html");
        var fresh = doc.querySelector(sel);
        if (!fresh) { location.href = u.href; return; }
        var dst = el.querySelector("[data-append]");
        var src = fresh.querySelector("[data-append]");
        if (dst && src) {
          var hasData = src.children.length > 0 && !src.querySelector('.empty-products, .empty-state, [class*="empty"]');
          if (hasData) {
            Array.prototype.slice.call(src.children).forEach(function(c) {
              dst.appendChild(document.importNode(c, true));
            });
          }
        } else {
          el.innerHTML = fresh.innerHTML;
        }
        var nb = fresh.querySelector("[data-load-more]");
        if (nb) btn.replaceWith(document.importNode(nb, true));
        else btn.remove();
        watchIfNeeded(sel);
      })
      .catch(function(e) {
        console.error("[VLO AJAX more]", e);
        btn.innerText = originalText;
        btn.style.opacity = "1";
      })
      .then(function() { V.busy[sel] = false; });
  }

  function watchIfNeeded(sel) {
    var el = document.querySelector(sel);
    var b = el ? el.querySelector("[data-load-more][data-auto]") : null;
    if (!b || !("IntersectionObserver" in window)) return;
    if (!V.io) {
      V.io = new IntersectionObserver(function(entries) {
        entries.forEach(function(en) {
          if (!en.isIntersecting) return;
          V.io.unobserve(en.target);
          var s = owner(en.target);
          if (s) loadMore(s);
        });
      }, { rootMargin: "300px" });
    }
    V.io.observe(b);
  }

  window.__VLO_AJAX_LOADMORE__ = loadMore;
  window.__VLO_AJAX_WATCH__ = watchIfNeeded;
})();
"##;

// ── Kept for backward compat / ajax_js_handler route ──
pub const AJAX_CORE_JS: &str = r##"/* replaced by modular injection */"##;

fn js_lit(s: &str) -> String {
    serde_json::to_string(s)
        .unwrap_or_else(|_| "\"\"".into())
        .replace("</", "<\\/")
}


// ── AJAX Modifier ──────────────────────────────────────────────────────
fn m_ajax(v: String, arg: Option<String>) -> String {
    let arg_str = arg.unwrap_or_default();
    let parts: Vec<&str> = arg_str.split(',').map(|s| s.trim()).collect();
    if parts.is_empty() { return v; }
    
    let url = parts.first().map(|s| s.trim()).unwrap_or("");
    let interval = parts.get(1).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let target = parts.get(2).unwrap_or(&"").trim();
    let mode = parts.get(3).unwrap_or(&"json").trim().to_lowercase();
    
    if mode == "html" {
        if target.is_empty() { return v; }
        // 🔥 Tiny queue push — core script injected once by wrap_html
        return format!(
            r#"<script>(window.__VLO_AJAX_Q=window.__VLO_AJAX_Q||[]).push({{sel:{},url:{},interval:{}}});</script>"#,
            js_lit(target),
            js_lit(url),
            interval
        );
    }
    
    // JSON MODE (unchanged — small, per-element, no queue needed)
    let safe_class = if url.is_empty() {
        "vlo-ajax-listen".to_string()
    } else {
        format!("vlo-ajax-{}", url.replace('/', "-").replace('?', "-").replace('&', "-").trim_matches('-'))
    };

    format!(
        r#"<span class="{}" data-url="{}" data-interval="{}" data-field="{}">{}</span><script>
    (function() {{
        function doFetch(el) {{
            fetch(el.dataset.url, {{credentials:'same-origin'}})
                .then(r => r.json())
                .then(res => {{
                    let val = res;
                    if (el.dataset.field) {{
                        val = el.dataset.field.split('.').reduce((o, k) => (o || {{}})[k], res);
                    }} else if (res.data && Array.isArray(res.data) && res.data.length > 0) {{
                        val = res.data[0];
                    }} else if (res.pagination && res.pagination.total !== undefined) {{
                        val = res.pagination.total;
                    }}
                    if (val !== undefined && val !== null) {{
                        el.innerText = typeof val === 'object' ? JSON.stringify(val) : val;
                        el.classList.add('vlo-ajax-updated');
                        setTimeout(() => el.classList.remove('vlo-ajax-updated'), 600);
                    }}
                }}).catch(e => console.error('[VLO AJAX]', e));
        }}
        document.querySelectorAll('.{}').forEach(el => {{
            doFetch(el);
            if (el.dataset.interval > 0) setInterval(() => doFetch(el), el.dataset.interval);
        }});
    }})();
    </script>"#,
        safe_class, url, interval, target, v, safe_class
    )
}
// ── Developer Experience ─────────────────────────────────────────────
fn m_debug(v: String, arg: Option<String>) -> String {
    let label = arg.unwrap_or_else(|| "DEBUG".into());
    eprintln!("[VLO {}] value={:?} len={}", label, v, v.chars().count());
    v
}
fn m_log(v: String, arg: Option<String>) -> String {
    println!("[VLO LOG {}] {}", arg.unwrap_or_else(|| "value".into()), v);
    v
}

// ── Registry ─────────────────────────────────────────────────────────
pub fn registry() -> HashMap<&'static str, ModifierFn> {
    // Explicitly typing the HashMap forces all inserted functions to 
    // be coerced into the `ModifierFn` function pointer type.
    let mut m: HashMap<&'static str, ModifierFn> = HashMap::new();

    // Text
    m.insert("upper", m_upper);
    m.insert("uppercase", m_upper);
    m.insert("lower", m_lower);
    m.insert("lowercase", m_lower);
    m.insert("proper", m_proper);
    m.insert("title", m_proper);
    m.insert("capitalize", m_capitalize);
    m.insert("ucfirst", m_capitalize);
    m.insert("lcfirst", m_lcfirst);
    m.insert("trim", m_trim);
    m.insert("ltrim", m_ltrim);
    m.insert("rtrim", m_rtrim);
    m.insert("slug", m_slug);
    m.insert("reverse", m_reverse);
    m.insert("truncate", m_truncate);
    m.insert("truncate_words", m_truncate_words);
    m.insert("replace", m_replace);
    m.insert("pad", m_pad);
    m.insert("words", m_words);
    m.insert("len", m_len);
    m.insert("length", m_len);
    m.insert("string", m_string);
    m.insert("str", m_string);
    m.insert("escape", m_escape);
    m.insert("nl2br", m_nl2br);
    m.insert("upper_words", m_upper_words);

    // Number
    m.insert("number", m_number);
    m.insert("double", m_double);
    m.insert("float", m_double);
    m.insert("int", m_int);
    m.insert("integer", m_int);
    m.insert("currency", m_currency);
    m.insert("percent", m_percent);

    // Date / time
    m.insert("format", m_format);
    m.insert("year", m_year);
    m.insert("yr", m_year);
    m.insert("month", m_month);
    m.insert("mon", m_month);
    m.insert("month_name", m_month_name);
    m.insert("monthname", m_month_name);
    m.insert("month_short", m_month_short);
    m.insert("mon_short", m_month_short);
    m.insert("quarter", m_quarter);
    m.insert("qtr", m_quarter);
    m.insert("q", m_quarter);
    m.insert("week", m_week);
    m.insert("wk", m_week);
    m.insert("week_number", m_week);
    m.insert("day", m_day);
    m.insert("date", m_date);
    m.insert("date_only", m_day);
    m.insert("day_name", m_day_name);
    m.insert("weekday", m_day_name);
    m.insert("day_short", m_day_short);
    m.insert("weekday_short", m_day_short);
    m.insert("hour", m_hour);
    m.insert("minute", m_minute);
    m.insert("min", m_minute);
    m.insert("time", m_time);
    m.insert("datetime", m_datetime);
    m.insert("timeago", m_timeago);

    // Type / logic
    m.insert("default", m_default);

    // Security
    m.insert("mask", m_mask);
    m.insert("hash", m_hash);

    // JSON / URL
    m.insert("json", m_json);
    m.insert("json_path", m_json_path);
    m.insert("query", m_query);

    // HTML / content
    m.insert("strip_tags", m_strip_tags);
    m.insert("excerpt", m_excerpt);
    m.insert("image", m_image);       // ← Added in Priority 2
    m.insert("live", m_live);         // 🔥 NEW: Real-time SSE updates
    m.insert("ajax", m_ajax);

    // Developer tools
    m.insert("debug", m_debug);
    m.insert("log", m_log);

    m
}

pub static REG: LazyLock<HashMap<&'static str, ModifierFn>> = LazyLock::new(registry);

// ── Public entry point ───────────────────────────────────────────────
pub fn apply(value: Value, chain: &str) -> String {
    let original = value.clone();
    let mut current = value_to_string(&value);

    for (name, arg) in parse_chain(chain) {
        match name.as_str() {
            "type" => current = value_type(&original).into(),
            "is" => current = if value_is(&original, arg.as_deref().unwrap_or("")) { "true" } else { "false" }.into(),
            _ => if let Some(f) = REG.get(name.as_str()) {
                current = f(current, arg);
            }
        }
    }
    current
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

// ── Modifier parser: supports `|month`, `|format:"%Y-%m"`, and chains ─
fn parse_chain(chain: &str) -> Vec<(String, Option<String>)> {
    let chain = chain.trim().trim_start_matches('|');
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote = None;

    for c in chain.chars() {
        match c {
            '"' | '\'' => {
                if quote == Some(c) { quote = None; }
                else if quote.is_none() { quote = Some(c); }
                current.push(c);
            }
            '|' if quote.is_none() => {
                if !current.trim().is_empty() { parts.push(current.trim().to_string()); }
                current.clear();
            }
            _ => current.push(c),
        }
    }
    if !current.trim().is_empty() { parts.push(current.trim().to_string()); }

    parts.into_iter().map(|part| {
        let mut p = part.splitn(2, ':');
        let name = p.next().unwrap_or("").trim().to_lowercase();
        let arg = p.next().map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string());
        (name, arg)
    }).collect()
}