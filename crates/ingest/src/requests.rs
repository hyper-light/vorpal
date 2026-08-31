//! Link-time request → route matching (ADOPTION #25 slice 2): HTTP client call sites with
//! literal URLs (see `RequestSpec` in `references.rs`) are matched against the `Route`
//! nodes' `VERB /path` templates, and each UNIQUE match becomes a directional `requests`
//! edge from the calling definition to the route. Ambiguity refuses and is counted;
//! unmatched sites are counted (external services are normal); nothing is guessed.
//!
//! Path matching: the request URL is stripped to its path (scheme + host removed, query
//! and fragment dropped), both sides split on `/` with empty segments removed; a route
//! template segment in parameter form (`:id`, `{id}`, `<int:id>`, `*`, mqtt `+`) matches
//! any one request segment, and every literal segment must match exactly. A route whose
//! verb is `ROUTE` (unknown in the source) accepts any request method. Confidence: 95
//! when every segment matched literally, 85 when parameters absorbed segments.
//!
//! Channels ride the same machinery with `EVENT` on both sides: an emitter record
//! (`emit("user.created")`) matches ONLY `EVENT <topic>` registrations, and — pub/sub
//! being one-to-many by design — links to EVERY matching listener via `notifies`
//! (confidence 90), capped at [`MAX_FANOUT`] per site and counted beyond.

/// One replayed request record (spill row).
#[derive(Debug, Clone)]
pub(crate) struct ReqRow {
  pub(crate) from: u64,
  pub(crate) method: Box<str>,
  pub(crate) path: Box<str>,
  pub(crate) span: (u32, u32),
}

/// Listener registrations one emitter site may link to before the fan-out is capped.
const MAX_FANOUT: usize = 16;

/// What the pass did, for the index report.
#[derive(Debug, Default, Clone)]
pub struct RequestReport {
  /// Client call sites with a literal URL.
  pub sites: u64,
  /// Directional `requests` edges sealed.
  pub edges: u64,
  /// Sites whose URL matched no route — external services, or routes outside this tree.
  pub unmatched: u64,
  /// Sites whose URL matched MORE than one route: refused, never guessed. (Event topics
  /// are exempt — fan-out is their semantics.)
  pub ambiguous: u64,
  /// Emitter sites whose listener fan-out exceeded the cap; the first [`MAX_FANOUT`]
  /// linked, the rest did not — counted, never silent.
  pub fanout_capped: u64,
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
    || text == "+"
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
  // Channels and routes never cross: an EVENT record only matches an EVENT registration.
  let event_template = template.verb.as_ref() == "EVENT";
  if event_template != (method == "EVENT") {
    return false;
  }
  if !event_template && template.verb.as_ref() != "ROUTE" && template.verb.as_ref() != method {
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

/// The matcher's sealed pairs, split by edge family: HTTP requests → `requests`,
/// event emitters → `notifies`. Both `(from, target, confidence)`, deterministic order.
#[derive(Debug, Default)]
pub(crate) struct MatchedEdges {
  pub(crate) requests: Vec<(u64, u64, u8)>,
  pub(crate) notifies: Vec<(u64, u64, u8)>,
}

/// Directional edges per family, deterministic order, plus the report.
pub(crate) fn match_requests(routes: &[(u64, String)], rows: &[ReqRow]) -> (MatchedEdges, RequestReport) {
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
        "{} request/emit sites, but this tree defines no routes or channels — all external",
        report.sites
      ));
    }
    return (MatchedEdges::default(), report);
  }
  let mut ordered: Vec<&ReqRow> = rows.iter().collect();
  ordered.sort_by_key(|r| (r.from, r.span));
  let mut edges = MatchedEdges::default();
  for row in ordered {
    let segments = split_segments(request_path(&row.path));
    if row.method.as_ref() == "EVENT" {
      // Pub/sub: every matching registration links (bounded), in template order.
      let hits: Vec<&Template> = templates
        .iter()
        .filter(|t| matches(t, &row.method, &segments))
        .collect();
      if hits.is_empty() {
        report.unmatched += 1;
        continue;
      }
      if hits.len() > MAX_FANOUT {
        report.fanout_capped += 1;
      }
      for template in hits.into_iter().take(MAX_FANOUT) {
        edges.notifies.push((row.from, template.node, 90));
      }
      continue;
    }
    let mut hits = templates
      .iter()
      .filter(|t| matches(t, &row.method, &segments));
    match (hits.next(), hits.next()) {
      (Some(only), None) => {
        let confidence = if only.literal_only { 95 } else { 85 };
        edges.requests.push((row.from, only.node, confidence));
      }
      (Some(_), Some(_)) => report.ambiguous += 1,
      (None, _) => report.unmatched += 1,
    }
  }
  for list in [&mut edges.requests, &mut edges.notifies] {
    list.sort_unstable();
    list.dedup_by_key(|edge| (edge.0, edge.1));
  }
  report.edges = (edges.requests.len() + edges.notifies.len()) as u64;
  if report.edges == 0 && report.sites > 0 {
    report.note = Some(format!(
      "{} request/emit sites, none linked ({} unmatched, {} ambiguous)",
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
      edges.requests,
      vec![(1, 10, 85), (2, 11, 95), (3, 12, 95), (4, 13, 95)],
      "{report:?}"
    );
    assert!(edges.notifies.is_empty());
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
    assert!(edges.requests.is_empty() && edges.notifies.is_empty());
    assert_eq!(report.ambiguous, 1);
    assert!(report.note.as_deref().unwrap().contains("1 ambiguous"), "{report:?}");
  }

  #[test]
  fn event_topics_fan_out_and_never_cross_into_routes() {
    let routes = vec![
      (10, "EVENT user.created".to_string()),
      (11, "EVENT user.created".to_string()),
      (12, "ROUTE /user.created".to_string()),
      (13, "EVENT user.*".to_string()),
    ];
    let rows = vec![row(1, "EVENT", "user.created"), row(2, "GET", "/user.created")];
    let (edges, report) = match_requests(&routes, &rows);
    // The emitter links BOTH listeners (and never the ROUTE); the GET links the route only.
    assert_eq!(edges.notifies, vec![(1, 10, 90), (1, 11, 90)], "{report:?}");
    assert_eq!(edges.requests, vec![(2, 12, 95)], "{report:?}");
    assert_eq!(report.edges, 3);
    assert_eq!(report.ambiguous, 0);
    assert_eq!(report.fanout_capped, 0);
  }

  #[test]
  fn no_routes_is_stated() {
    let (edges, report) = match_requests(&[], &[row(1, "GET", "/x")]);
    assert!(edges.requests.is_empty() && edges.notifies.is_empty());
    assert!(report.note.as_deref().unwrap().contains("defines no routes or channels"));
  }
}
