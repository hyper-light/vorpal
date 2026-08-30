[
  (message_body)
  (enum_body)
  (oneof)
  (service)
] @indent.begin

; rpc's body is optional (it can end in ";" instead of "{ }"), so anchor on
; the brace itself rather than the whole node to avoid indenting past a
; semicolon-terminated rpc.
(rpc
  "{" @indent.begin)

"}" @indent.end @indent.branch

[
  (ERROR)
  (comment)
] @indent.auto
