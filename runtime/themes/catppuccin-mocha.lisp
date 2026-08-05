;;;; catppuccin-mocha — the darkest of the four Catppuccin flavours
;;;;
;;;; Catppuccin is a pastel palette with a published style guide that says which
;;;; of its twenty-six colours every kind of token gets, so porting it is mostly
;;;; a matter of not arguing with the guide. The values below are the ones in
;;;; `catppuccin/palette', and the assignments are the guide's and
;;;; `catppuccin/emacs': keywords are Mauve, types Yellow, functions Blue,
;;;; strings Green, constants and numbers Peach, operators Sky, delimiters
;;;; Overlay 2, and headings walk the rainbow Red, Peach, Yellow.
;;;;
;;;; Load it from your init.lisp — these are ordinary top-level forms, so
;;;; loading the file *is* applying the theme:
;;;;
;;;;   (load-theme "catppuccin-mocha")
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. A face left out here is a face the previous theme still
;;;; owns — and since faces carry weight now as well as colour, it is a face
;;;; that keeps the previous theme's *bold* too, which is the worse half of the
;;;; bug. That is the whole reason the list is exhaustive and boring.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse
;;;; these. A directory is `type', an executable `function', a symlink `link',
;;;; a staged file `string' and an unstaged one `keyword'. So the same 22 lines
;;;; that colour a source buffer colour those too.
;;;;
;;;; On weight: five faces are bold and two are italic, and that is deliberately
;;;; near the ceiling — a screen where everything is heavy has no emphasis in it
;;;; at all. Catppuccin italicises comments in every port that offers the switch
;;;; (`styles.comments' in the nvim theme, on by default;
;;;; `catppuccin-italic-comments' in the Emacs one, off by default — this file
;;;; takes the italic) and bolds nothing in font-lock at all: keyword, type and
;;;; function are carried by hue alone upstream, and are left that way here
;;;; rather than invented into boldness. The weight goes where upstream puts it,
;;;; which is structure — headings and the bar at the bottom of the window.
;;;;
;;;; Five mappings are not Catppuccin's own:
;;;;
;;;; * `heading-3' — upstream's `org-level-3' is :weight normal, where levels 1
;;;;   and 2 inherit bold. zemacs paints level 3 and everything below it in the
;;;;   same face, so it is the last rung rather than the middle of a ladder, and
;;;;   three heading levels that differ only in hue read as three differently-
;;;;   coloured paragraphs rather than as an outline.
;;;; * `modeline' and `modeline-inactive' — catppuccin/emacs draws the active
;;;;   mode line on Mantle and the inactive one on Crust, a pair separated by
;;;;   seven values out of 255. That works in Emacs because the two also differ
;;;;   in foreground; zemacs writes both bars with one `modeline-text', so the
;;;;   bar itself has to say which window has focus. They take the Surface ramp
;;;;   instead, which is where the style guide puts UI chrome anyway, with the
;;;;   active bar one step further from Base than the inactive one.
;;;; * `modeline-text' — bold. Emacs bolds the parts of the mode line that carry
;;;;   the information (`mode-line-buffer-id', `mode-line-emphasis') and leaves
;;;;   the rest plain; with a single face for the whole bar the choice is all or
;;;;   nothing, and bold is what keeps the bar from reading as one more line of
;;;;   the buffer.
;;;; * `markup' — the emphasis delimiters, the `**' around a bold word. Upstream
;;;;   has no face for them: `org-meta-line' inherits the comment face, which
;;;;   would make the asterisks exactly as loud as the word between them. They
;;;;   take Overlay 1, one step below the Overlay 2 the guide gives comments.
;;;; * `variable' — Text rather than an accent, which *is* upstream's
;;;;   `font-lock-variable-name-face': Catppuccin colours what an identifier
;;;;   specifically is — a property, a parameter, a type — and leaves a plain one
;;;;   in the body colour. The deviation is the other half of the face, since
;;;;   zemacs also spends `variable' on tree-sitter's `property' captures, which
;;;;   the guide paints Blue. The plain case is much the commoner, so it wins.

(in-package :zemacs)

;;; The palette, named as Catppuccin names it.
(let (;; Base shades and the surfaces drawn on top of them
      (base     '(0.118 0.118 0.180))  ; #1e1e2e
      (surface1 '(0.271 0.278 0.353))  ; #45475a
      (surface0 '(0.192 0.196 0.267))  ; #313244

      ;; Text, and the overlays that step down from it
      (text     '(0.804 0.839 0.957))  ; #cdd6f4
      (overlay2 '(0.576 0.600 0.698))  ; #9399b2
      (overlay1 '(0.498 0.518 0.612))  ; #7f849c

      ;; Accents
      (mauve    '(0.796 0.651 0.969))  ; #cba6f7
      (red      '(0.953 0.545 0.659))  ; #f38ba8
      (peach    '(0.980 0.702 0.529))  ; #fab387
      (yellow   '(0.976 0.886 0.686))  ; #f9e2af
      (green    '(0.651 0.890 0.631))  ; #a6e3a1
      (sky      '(0.537 0.863 0.922))  ; #89dceb
      (blue     '(0.537 0.706 0.980))  ; #89b4fa
      (lavender '(0.706 0.745 0.996))  ; #b4befe
      )

  (apply #'set-background base)
  (apply #'set-foreground text)

  ;; All 22 of them. Anything skipped keeps the last theme's colour and weight.
  (set-face "default"           text)                 ; #cdd6f4  `default'
  (set-face "keyword"           mauve)                ; #cba6f7  Keywords
  (set-face "function"          blue)                 ; #89b4fa  Methods, Functions
  (set-face "type"              yellow)               ; #f9e2af  Classes, Types
  (set-face "string"            green)                ; #a6e3a1  Strings
  (set-face "number"            peach)                ; #fab387  Constants, Numbers
  (set-face "comment"           overlay2 :italic t)   ; #9399b2  Comments
  (set-face "constant"          peach)                ; #fab387  Constants, Numbers
  (set-face "variable"          text)                 ; #cdd6f4  `font-lock-variable-name-face'
  (set-face "operator"          sky)                  ; #89dceb  Operators
  (set-face "punctuation"       overlay2)             ; #9399b2  Braces, Delimiters
  (set-face "heading-1"         red      :bold t)     ; #f38ba8  `org-level-1'
  (set-face "heading-2"         peach    :bold t)     ; #fab387  `org-level-2'
  (set-face "heading-3"         yellow   :bold t)     ; #f9e2af  `org-level-3'
  (set-face "bold"              text     :bold t)     ; #cdd6f4  Emacs' own `bold'
  (set-face "italic"            text     :italic t)   ; #cdd6f4  Emacs' own `italic'
  (set-face "link"              lavender)             ; #b4befe  `link'
  (set-face "code"              green)                ; #a6e3a1  `org-code', `org-verbatim'
  (set-face "markup"            overlay1)             ; #7f849c  see the header
  (set-face "modeline"          surface1)             ; #45475a  bar, current window
  (set-face "modeline-inactive" surface0)             ; #313244  bar, other windows
  (set-face "modeline-text"     text     :bold t)     ; #cdd6f4  what is written on it
  )
