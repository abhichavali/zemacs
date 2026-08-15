;;;; ayu-mirage — ayu with the lights half up, the same hues over a blue-grey
;;;;
;;;; Mirage is the middle of ayu's three variants: the ground lifts off black to
;;;; a desaturated blue-grey and every accent lifts with it, so the theme keeps
;;;; its shape while losing the glare. The values come from `ayu-theme/ayu-colors'
;;;; at v8.0.1 — the palette repository every ayu port generates from — checked
;;;; line by line against `Shatur/neovim-ayu', which pins the same numbers.
;;;;
;;;;   (load-theme "ayu-mirage")
;;;;
;;;; Provenance matters here more than usual, because several palettes answer to
;;;; this name. `ayu-theme/ayu-vim' has been frozen since 2019 on the first
;;;; generation — ground #212733, body #d9d7ce, a cyan operator — and is not what
;;;; this file ports. Nor is the generation after it, whose keyword was #ffa759
;;;; and whose special was #ffc44c; both moved, to #ffad66 and #ffdfb3. And the
;;;; upstream default branch has since moved on again, to a computed palette with
;;;; a surface ramp, which is not ported either. v8 is where the ecosystem sits.
;;;;
;;;; ayu splits its ground in two: `editor.bg' #242936 for the text pane and
;;;; `ui.bg' #1f2430 for the chrome around it. zemacs has one ground and takes
;;;; `ui.bg', which is the value people mean by ayu mirage and the one the Neovim
;;;; port paints buffers with. One consequence is visible below. Mirage is the
;;;; variant whose other grounds all sit *under* `ui.bg' rather than above it:
;;;; `ui.line' #171b24, `editor.line' #1a1f29 and `ui.panel.bg' #1c212b are
;;;; opaque literals upstream, not blends, and every one of them is darker than
;;;; #1f2430. So the bars and the stripe are darker than the buffer here, where
;;;; in ayu dark the same three are all lighter than it. That inversion is
;;;; mirage's own and not an error.
;;;;
;;;; Four colours arrive with an alpha channel: the comment at 50%, the selection
;;;; band at 25%, and the two gutter greys at 40% and 80%. zemacs has no alpha,
;;;; so each was composited against this theme's own ground, #1f2430, and the
;;;; opaque result is what appears below. The arithmetic reproduces the Neovim
;;;; port's own pre-composited literals byte for byte.
;;;;
;;;; ayu's key names are used for the palette, with one exception: `string',
;;;; `keyword', `special' and `error' name symbols in COMMON-LISP and cannot be
;;;; bound as variables, so those four carry an `ayu-' prefix. The `regexp'
;;;; colour and the three version-control colours are not bound at all; zemacs
;;;; has no face for an escape sequence or a diff stat.
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. A face left out is a face the previous theme still owns — its
;;;; colour and, since faces carry weight too, its bold. Dired and the git status
;;;; buffer have no faces of their own either: a directory is `type', an
;;;; executable `function', a symlink `link', a staged file `string' and an
;;;; unstaged one `keyword'.
;;;;
;;;; On weight and slant: ayu italicises comments in every port and bolds no
;;;; font-lock group at all, and both are kept — the theme is carried by hue. Its
;;;; markdown rules do use weight, `markup.bold' and `markup.heading' both bold,
;;;; and those are kept too.
;;;;
;;;; Five mappings are not ayu's own — the same five as in the dark variant:
;;;;
;;;; * `heading-1/2/3' — the VS Code theme paints `markup.heading' the string
;;;;   green, bold; the Neovim port paints `@markup.heading' the keyword orange.
;;;;   zemacs needs three, so it takes both, then the accent.
;;;; * `punctuation' and `code' — both take `special', which is the Neovim port's
;;;;   colour for `Delimiter' and for `markdownCode'. In mirage that colour is a
;;;;   pale sand, so brackets stay quiet without going grey.
;;;; * `markup' — the `*' and `=' delimiters take `ui.fg', the grey ayu uses for
;;;;   chrome and for markdown's punctuation. Upstream has no group for them.
;;;; * `modeline' and `modeline-inactive' — ayu draws both status bars on the
;;;;   panel background and tells them apart by foreground. zemacs writes every
;;;;   bar with one `modeline-text', so the bar carries the difference:
;;;;   `editor.line' for the focused one, which sits furthest from the buffer,
;;;;   and the panel for the rest, which sits nearest. That one face for the
;;;;   whole bar is also why it is bold — emphasis is all or none.
;;;; * `current-line', `divider' and `popup-border' — ayu names three grounds
;;;;   below the body colour and zemacs asks for five, so two of them do double
;;;;   duty. `ui.line' is the stripe under point, as it is in the Neovim port,
;;;;   and it is also the rule between panes and the edge of a popup.

(in-package :zemacs)

;;; The palette, named as ayu names it.
(let (;; Grounds
      (bg           '(0.122 0.141 0.188))  ; #1f2430  ui.bg
      (panel        '(0.110 0.129 0.169))  ; #1c212b  ui.panel.bg
      (ui-line      '(0.090 0.106 0.141))  ; #171b24  ui.line
      (line         '(0.102 0.122 0.161))  ; #1a1f29  editor.line

      ;; Foregrounds
      (fg           '(0.800 0.792 0.761))  ; #cccac2  editor.fg
      (ui           '(0.439 0.478 0.549))  ; #707a8c  ui.fg
      (comment      '(0.424 0.478 0.545))  ; #6c7a8b  #b8cfe6 at 50%
      (gutter       '(0.290 0.314 0.353))  ; #4a505a  #8a9199 at 40%
      (gutter-on    '(0.459 0.482 0.518))  ; #757b84  #8a9199 at 80%

      ;; Bands
      (selection    '(0.153 0.263 0.392))  ; #274364  #409fff at 25%
      (find-match   '(0.412 0.325 0.502))  ; #695380  editor.findMatch.active

      ;; Accents
      (accent       '(1.000 0.800 0.400))  ; #ffcc66  common.accent
      (tag          '(0.361 0.812 0.902))  ; #5ccfe6
      (func         '(1.000 0.820 0.451))  ; #ffd173
      (entity       '(0.451 0.816 1.000))  ; #73d0ff
      (ayu-string   '(0.835 1.000 0.502))  ; #d5ff80
      (markup       '(0.949 0.529 0.475))  ; #f28779
      (ayu-keyword  '(1.000 0.678 0.400))  ; #ffad66
      (ayu-special  '(1.000 0.875 0.702))  ; #ffdfb3
      (constant     '(0.875 0.749 1.000))  ; #dfbfff
      (operator     '(0.949 0.620 0.455))  ; #f29e74
      (ayu-error    '(1.000 0.400 0.400))  ; #ff6666  common.error
      )

  (apply #'set-background bg)
  (apply #'set-foreground fg)

  ;; The 22 core faces. Anything skipped keeps the last theme's colour and weight.
  (set-face "default"           fg)                     ; #cccac2  Normal
  (set-face "keyword"           ayu-keyword)            ; #ffad66  Statement, keyword
  (set-face "function"          func)                   ; #ffd173  entity.name.function
  (set-face "type"              entity)                 ; #73d0ff  Type, Identifier
  (set-face "string"            ayu-string)             ; #d5ff80  String
  (set-face "number"            constant)               ; #dfbfff  constant.numeric
  (set-face "comment"           comment  :italic t)     ; #6c7a8b  Comment
  (set-face "constant"          constant)               ; #dfbfff  Constant
  (set-face "variable"          fg)                     ; #cccac2  @variable
  (set-face "operator"          operator)               ; #f29e74  keyword.operator
  (set-face "punctuation"       ayu-special)            ; #ffdfb3  Delimiter
  (set-face "heading-1"         ayu-string   :bold t)   ; #d5ff80  markup.heading
  (set-face "heading-2"         ayu-keyword  :bold t)   ; #ffad66  see the header
  (set-face "heading-3"         accent       :bold t)   ; #ffcc66  see the header
  (set-face "bold"              markup   :bold t)       ; #f28779  markup.bold
  (set-face "italic"            markup   :italic t)     ; #f28779  markup.italic
  (set-face "link"              tag)                    ; #5ccfe6  markup.underline.link
  (set-face "code"              ayu-special)            ; #ffdfb3  markdownCode
  (set-face "markup"            ui)                     ; #707a8c  see the header
  (set-face "modeline"          line)                   ; #1a1f29  bar, current window
  (set-face "modeline-inactive" panel)                  ; #1c212b  bar, other windows
  (set-face "modeline-text"     fg       :bold t)       ; #cccac2  what is written on it

  ;; The UI faces. Each would fall back to a mix of the ground and the body
  ;; colour if it were left out; ayu names all twelve.
  (set-face "region"             selection)             ; #274364  editor.selection.active
  (set-face "cursor"             accent)                ; #ffcc66  editorCursor.foreground
  (set-face "current-line"       ui-line)               ; #171b24  CursorLine
  (set-face "line-number"        gutter)                ; #4a505a  editor.gutter.normal
  (set-face "line-number-current" gutter-on)            ; #757b84  editor.gutter.active
  (set-face "divider"            ui-line)               ; #171b24  editorGroup.border
  (set-face "error"              ayu-error)             ; #ff6666  DiagnosticError
  (set-face "warning"            ayu-keyword)           ; #ffad66  DiagnosticWarn
  (set-face "match"              find-match)            ; #695380  editor.findMatch.active
  (set-face "popup"              panel)                 ; #1c212b  suggest widget fill
  (set-face "popup-border"       ui-line)               ; #171b24  suggest widget edge
  (set-face "accent"             accent)                ; #ffcc66  common.accent
  )
