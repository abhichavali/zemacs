;;;; ayu-dark — Ike Ku's ayu, the deep variant, gold on something close to ink
;;;;
;;;; ayu is a palette rather than a plugin: `ayu-theme/ayu-colors' is the source
;;;; of truth and every editor port generates from it. The values below are that
;;;; repository at v8.0.1, cross-checked line by line against
;;;; `Shatur/neovim-ayu', which is the maintained Neovim port and pins the same
;;;; palette as literals.
;;;;
;;;;   (load-theme "ayu-dark")
;;;;
;;;; A warning about provenance, because two very different palettes answer to
;;;; the same name. `ayu-theme/ayu-vim' has been frozen since 2019 on the first
;;;; generation — ground #0f1419, body #e6e1cf, a yellow operator — and is not
;;;; what this file ports. Modern ayu, the one in VS Code and Sublime and every
;;;; current port, is the palette here. The v8 line is also not the newest: the
;;;; upstream default branch has since moved the ground to #10141c and added a
;;;; surface ramp. v8 is what the ecosystem is actually on.
;;;;
;;;; ayu splits its ground in two: `editor.bg' #0d1017 for the text pane and
;;;; `ui.bg' #0b0e14 for the chrome around it. zemacs has one ground and takes
;;;; `ui.bg', which is the value people mean by ayu dark and the one the Neovim
;;;; port paints buffers with.
;;;;
;;;; Four colours arrive with an alpha channel: the comment at 55%, the selection
;;;; band at 30%, and the two gutter greys at 60% and 90%. zemacs has no alpha,
;;;; so each was composited against this theme's own ground, #0b0e14, and the
;;;; opaque result is what appears below. Three of the four land on the Neovim
;;;; port's own pre-composited literals exactly; the comment misses by one unit
;;;; of red, because 0.55 of #acb6bf over #0b0e14 comes to 99.55 and the port
;;;; rounds it down. The port's #636a72 is what is written below — matching what
;;;; ayu users actually see beats matching the arithmetic.
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
;;;; font-lock group at all, and both are kept — the theme is carried by hue.
;;;; Its markdown rules do use weight, `markup.bold' bold and `markup.heading'
;;;; bold, and those are kept too.
;;;;
;;;; Five mappings are not ayu's own:
;;;;
;;;; * `heading-1/2/3' — the two reference-grade implementations disagree. The VS
;;;;   Code theme paints `markup.heading' the string green, bold; the Neovim port
;;;;   paints `@markup.heading' the keyword orange. zemacs needs three levels, so
;;;;   it takes the VS Code answer, then the Neovim one, then the accent.
;;;; * `punctuation' and `code' — both take `special', which is the Neovim port's
;;;;   colour for `Delimiter' and for `markdownCode'. The VS Code theme instead
;;;;   leaves brackets in the body colour and gives inline code the operator
;;;;   orange, which is the louder of the two readings.
;;;; * `markup' — the `*' and `=' delimiters take `ui.fg', the grey ayu uses for
;;;;   chrome and for markdown's own punctuation. Upstream has no single group
;;;;   for them.
;;;; * `modeline' and `modeline-inactive' — ayu draws both status bars on the
;;;;   panel background and tells them apart by foreground. zemacs writes every
;;;;   bar with one `modeline-text', so the bar carries the difference:
;;;;   `editor.line' for the focused one, the panel for the rest. `modeline-text'
;;;;   is bold because one face for the whole bar makes emphasis all or nothing.
;;;; * `current-line', `divider' and `popup-border' — ayu names three grounds
;;;;   below the body colour and zemacs asks for five, so two do double duty.
;;;;   `ui.line' is the stripe under point, as it is in the Neovim port, and it
;;;;   is also the rule between panes and the edge of a popup, which is what VS
;;;;   Code's editor-group and suggest-widget borders use it for.

(in-package :zemacs)

;;; The palette, named as ayu names it.
(let (;; Grounds
      (bg           '(0.043 0.055 0.078))  ; #0b0e14  ui.bg
      (panel        '(0.059 0.075 0.102))  ; #0f131a  ui.panel.bg
      (ui-line      '(0.067 0.082 0.110))  ; #11151c  ui.line
      (line         '(0.075 0.090 0.129))  ; #131721  editor.line

      ;; Foregrounds
      (fg           '(0.749 0.741 0.714))  ; #bfbdb6  editor.fg
      (ui           '(0.337 0.357 0.400))  ; #565b66  ui.fg
      (comment      '(0.388 0.416 0.447))  ; #636a72  #acb6bf at 55%
      (gutter       '(0.271 0.294 0.333))  ; #454b55  #6c7380 at 60%
      (gutter-on    '(0.384 0.412 0.459))  ; #626975  #6c7380 at 90%

      ;; Bands
      (selection    '(0.106 0.227 0.357))  ; #1b3a5b  #409fff at 30%
      (find-match   '(0.424 0.349 0.502))  ; #6c5980  editor.findMatch.active

      ;; Accents
      (accent       '(0.902 0.706 0.314))  ; #e6b450  common.accent
      (tag          '(0.224 0.729 0.902))  ; #39bae6
      (func         '(1.000 0.706 0.329))  ; #ffb454
      (entity       '(0.349 0.761 1.000))  ; #59c2ff
      (ayu-string   '(0.667 0.851 0.298))  ; #aad94c
      (markup       '(0.941 0.443 0.471))  ; #f07178
      (ayu-keyword  '(1.000 0.561 0.251))  ; #ff8f40
      (ayu-special  '(0.902 0.714 0.451))  ; #e6b673
      (constant     '(0.824 0.651 1.000))  ; #d2a6ff
      (operator     '(0.949 0.588 0.408))  ; #f29668
      (ayu-error    '(0.851 0.341 0.341))  ; #d95757  common.error
      )

  (apply #'set-background bg)
  (apply #'set-foreground fg)

  ;; The 22 core faces. Anything skipped keeps the last theme's colour and weight.
  (set-face "default"           fg)                     ; #bfbdb6  Normal
  (set-face "keyword"           ayu-keyword)            ; #ff8f40  Statement, keyword
  (set-face "function"          func)                   ; #ffb454  entity.name.function
  (set-face "type"              entity)                 ; #59c2ff  Type, Identifier
  (set-face "string"            ayu-string)             ; #aad94c  String
  (set-face "number"            constant)               ; #d2a6ff  constant.numeric
  (set-face "comment"           comment  :italic t)     ; #636a72  Comment
  (set-face "constant"          constant)               ; #d2a6ff  Constant
  (set-face "variable"          fg)                     ; #bfbdb6  @variable
  (set-face "operator"          operator)               ; #f29668  keyword.operator
  (set-face "punctuation"       ayu-special)            ; #e6b673  Delimiter
  (set-face "heading-1"         ayu-string   :bold t)   ; #aad94c  markup.heading
  (set-face "heading-2"         ayu-keyword  :bold t)   ; #ff8f40  see the header
  (set-face "heading-3"         accent       :bold t)   ; #e6b450  see the header
  (set-face "bold"              markup   :bold t)       ; #f07178  markup.bold
  (set-face "italic"            markup   :italic t)     ; #f07178  markup.italic
  (set-face "link"              tag)                    ; #39bae6  markup.underline.link
  (set-face "code"              ayu-special)            ; #e6b673  markdownCode
  (set-face "markup"            ui)                     ; #565b66  see the header
  (set-face "modeline"          line)                   ; #131721  bar, current window
  (set-face "modeline-inactive" panel)                  ; #0f131a  bar, other windows
  (set-face "modeline-text"     fg       :bold t)       ; #bfbdb6  what is written on it

  ;; The UI faces. Each would fall back to a mix of the ground and the body
  ;; colour if it were left out; ayu names all twelve.
  (set-face "region"             selection)             ; #1b3a5b  editor.selection.active
  (set-face "cursor"             accent)                ; #e6b450  editorCursor.foreground
  (set-face "current-line"       ui-line)               ; #11151c  CursorLine
  (set-face "line-number"        gutter)                ; #454b55  editor.gutter.normal
  (set-face "line-number-current" gutter-on)            ; #626975  editor.gutter.active
  (set-face "divider"            ui-line)               ; #11151c  editorGroup.border
  (set-face "error"              ayu-error)             ; #d95757  DiagnosticError
  (set-face "warning"            ayu-keyword)           ; #ff8f40  DiagnosticWarn
  (set-face "match"              find-match)            ; #6c5980  editor.findMatch.active
  (set-face "popup"              panel)                 ; #0f131a  suggest widget fill
  (set-face "popup-border"       ui-line)               ; #11151c  suggest widget edge
  (set-face "accent"             accent)                ; #e6b450  common.accent
  )
