; Frontmatter → TypeScript
(frontmatter
  (frontmatter_js_block) @injection.content
  (#set! injection.language "typescript")
  (#set! injection.combined))

; Script content → TypeScript
(script_element
  (raw_text) @injection.content
  (#set! injection.language "typescript")
  (#set! injection.combined))

; Style content → CSS
(style_element
  (raw_text) @injection.content
  (#set! injection.language "css")
  (#set! injection.combined))

; Attribute expressions → TypeScript
(attribute_interpolation
  (attribute_js_expr) @injection.content
  (#set! injection.language "typescript")
  (#set! injection.combined))

; HTML interpolations → TypeScript
(html_interpolation
  (permissible_text) @injection.content
  (#set! injection.language "typescript")
  (#set! injection.combined))
