//! Link-time request → route matching (ADOPTION #25 slice 2): HTTP client call sites with
//! literal URLs (see `RequestSpec` in `references.rs`) are matched against the `Route`
//! nodes' `VERB /path` templates, and each UNIQUE match becomes a directional `requests`
//! edge from the calling definition to the route. Ambiguity refuses and is counted;
//! unmatched sites are counted (external services are normal); nothing is guessed.
//!
//! Path matching: the request URL is stripped to its path (scheme + host removed, query
//! and fragment dropped), both sides split on `/` with empty segments removed; a route
//! template segment in parameter form (`:id`, `{id}`, `<int:id>`, `*`) matches any one
//! request segment, and every literal segment must match exactly. A route whose verb is
//! `ROUTE` (unknown in the source) accepts any request method. Confidence: 95 when every
//! segment matched literally, 85 when parameters absorbed segments.

/// One replayed request record (spill row).
#[derive(Debug, Clone)]
pub(crate) struct ReqRow {
  pub(crate) from: u64,
  pub(crate) method: Box<str>,
  pub(crate) path: Box<str>,
  pub(crate) span: (u32, u32),
}

/// What the pass did, for the index report.
#[derive(Debug, Default, Clone)]
pub struct RequestReport {
  /// Client call sites with a literal URL.
  pub sites: u64,
  /// Directional `requests` edges sealed.
  pub edges: u64,
  /// Sites whose URL matched no route — external services, or routes outside this tree.
  pub unmatched: u64,
  /// Sites whose URL matched MORE than one route: refused, never guessed.
  pub ambiguous: u64,
  /// Stated when something kept the pass from linking anything; `None` when edges exist
  /// or there was nothing to do.
  pub note: Option<String>,
}

/// A parsed route template.
struct Template {
  node: u64,
  verb: Box<str>,
  segments: Vec<Segment>,
  literal_only: bool,
}

enum Segment {
  Literal(Box<str>),
  Param,
}

fn parameter_segment(text: &str) -> bool {
  text.starts_with(':')
    || text.starts_with('*')
    || (text.starts_with('{') && text.ends_with('}'))
    || (text.starts_with('<') && text.ends_with('>'))
    || (text.starts_with('[') && text.ends_with(']'))
}

fn split_segments(path: &str) -> Vec<&str> {
  path.split('/').filter(|s| !s.is_empty()).collect()
}

/// The path part of a request URL: scheme + authority stripped, query/fragment dropped.
fn request_path(url: &str) -> &str {
  let after_host = if let Some(rest) = url.strip_prefix("http://").or_else(|| url.strip_prefix("https://")) {
    match rest.find('/') {
      Some(at) => &rest[at..],
      None => "/",
    }
  } else {
    url
  };
  let end = after_host
    .find(['?', '#'])
    .unwrap_or(after_host.len());
  &after_host[..end]
}

fn parse_route(node: u64, name: &str) -> Option<Template> {
  let (verb, path) = name.split_once(' ')?;
  if verb.is_empty() || !verb.bytes().all(|b| b.is_ascii_uppercase()) {
    return None;
  }
  let segments: Vec<Segment> = split_segments(path)
    .into_iter()
    .map(|segment| {
      if parameter_segment(segment) {
        Segment::Param
      } else {
        Segment::Literal(segment.into())
      }
    })
    .collect();
  let literal_only = segments.iter().all(|s| matches!(s, Segment::Literal(_)));
  Some(Template {
    node,
    verb: verb.into(),
    segments,
    literal_only,
  })
}

fn matches(template: &Template, method: &str, path_segments: &[&str]) -> bool {
  if template.verb.as_ref() != "ROUTE" && template.verb.as_ref() != method {
    return false;
  }
  if template.segments.len() != path_segments.len() {
    return false;
  }
  template
    .segments
    .iter()
    .zip(path_segments)
    .all(|(segment, actual)| match segment {
      Segment::Literal(text) => text.as_ref() == *actual,
      Segment::Param => true,
    })
}

/// Directional `(from, route, confidence)` edges, deterministic order, plus the report.
pub(crate) fn match_requests(
  routes: &[(u64, String)],
  rows: &[ReqRow],
) -> (Vec<(u64, u64, u8)>, RequestReport) {
  let mut report = RequestReport {
    sites: rows.len() as u64,
    ..RequestReport::default()
  };
  let templates: Vec<Template> = routes
    .iter()
    .filter_map(|(node, name)| parse_route(*node, name))
    .collect();
  if templates.is_empty() {
    report.unmatched = report.sites;
    if report.sites > 0 {
      report.note = Some(format!(
        "{} client call sites, but this tree defines no routes — all external",
        report.sites
      ));
    }
    return (Vec::new(), report);
  }
  let mut ordered: Vec<&ReqRow> = rows.iter().collect();
  ordered.sort_by_key(|r| (r.from, r.span));
  let mut edges: Vec<(u64, u64, u8)> = Vec::new();
  for row in ordered {
    let segments = split_segments(request_path(&row.path));
    let mut hits = templates
      .iter()
      .filter(|t| matches(t, &row.method, &segments));
    match (hits.next(), hits.next()) {
      (Some(only), None) => {
        let confidence = if only.literal_only { 95 } else { 85 };
        edges.push((row.from, only.node, confidence));
      }
      (Some(_), Some(_)) => report.ambiguous += 1,
      (None, _) => report.unmatched += 1,
    }
  }
  edges.sort_unstable();
  edges.dedup_by_key(|edge| (edge.0, edge.1));
  report.edges = edges.len() as u64;
  if report.edges == 0 && report.sites > 0 {
    report.note = Some(format!(
      "{} client call sites, none linked ({} unmatched, {} ambiguous)",
      report.sites, report.unmatched, report.ambiguous
    ));
  }
  (edges, report)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn row(from: u64, method: &str, path: &str) -> ReqRow {
    ReqRow {
      from,
      method: method.into(),
      path: path.into(),
      span: (0, 1),
    }
  }

  #[test]
  fn unique_matches_link_with_template_aware_confidence() {
    let routes = vec![
      (10, "GET /api/users/:id".to_string()),
      (11, "GET /api/users".to_string()),
      (12, "POST /api/users".to_string()),
      (13, "ROUTE /health".to_string()),
    ];
    let rows = vec![
      row(1, "GET", "/api/users/42"),
      row(2, "GET", "https://svc.example.com/api/users?limit=5"),
      row(3, "POST", "/api/users"),
      row(4, "DELETE", "/health"),
      row(5, "GET", "/nowhere"),
    ];
    let (edges, report) = match_requests(&routes, &rows);
    assert_eq!(
      edges,
      vec![(1, 10, 85), (2, 11, 95), (3, 12, 95), (4, 13, 95)],
      "{report:?}"
    );
    assert_eq!(report.sites, 5);
    assert_eq!(report.edges, 4);
    assert_eq!(report.unmatched, 1);
    assert_eq!(report.ambiguous, 0);
    assert!(report.note.is_none());
  }

  #[test]
  fn ambiguity_refuses_and_is_counted() {
    let routes = vec![
      (10, "GET /users/:id".to_string()),
      (11, "GET /users/{name}".to_string()),
    ];
    let (edges, report) = match_requests(&routes, &[row(1, "GET", "/users/42")]);
    assert!(edges.is_empty());
    assert_eq!(report.ambiguous, 1);
    assert!(report.note.as_deref().unwrap().contains("1 ambiguous"), "{report:?}");
  }

  #[test]
  fn no_routes_is_stated() {
    let (edges, report) = match_requests(&[], &[row(1, "GET", "/x")]);
    assert!(edges.is_empty());
    assert!(report.note.as_deref().unwrap().contains("defines no routes"));
  }
}
