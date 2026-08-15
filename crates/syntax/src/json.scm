; JSON. The grammar ships a `highlights.scm' and this is it reordered, which is
; the whole change: it captures a pair's key *before* it captures every string,
; and the last capture on a node wins, so the key came out the value's colour
; and an object read as one long green wall.
;
; The key takes `@type' rather than the stock `@string.special.key': the TOML
; query already paints a `bare_key' that way, and the two config formats looking
; alike is worth more than a capture name no face answers to.

(string) @string
(number) @number
(comment) @comment
(escape_sequence) @escape

[(null) (true) (false)] @constant.builtin

(pair key: (string) @type)
