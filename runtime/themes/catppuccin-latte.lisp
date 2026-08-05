;;;; catppuccin-latte — the light flavour, the one Catppuccin starts from
;;;;
;;;; Same theme as catppuccin-mocha and the same style guide; only the flavour
;;;; is different, and a flavour in Catppuccin is a whole second set of hexes
;;;; rather than a lightened first set. Latte is not Mocha inverted: its Mauve
;;;; is #8839ef where Mocha's is #cba6f7, its ramps run the other way (Overlay 0
;;;; is the *lightest* overlay here and the darkest there), and every value
;;;; below is the published Latte one from `catppuccin/palette'.
;;;;
;;;; Load it from your init.lisp — these are ordinary top-level forms, so
;;;; loading the file *is* applying the theme:
;;;;
;;;;   (load-theme "catppuccin-latte")
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
;;;; A word about contrast, since the Modus files sitting next to this one make
;;;; a point of it: Latte does not have that property and this port does not
;;;; pretend otherwise. Text on Base is 7:1, but the accents run from 4.8:1 for
;;;; Red down to 2.3:1 for Yellow — Catppuccin buys its pastel look with
;;;; contrast, and a Latte repainted until every hue passed WCAG would not be
;;;; Latte any more. What a port can do without lying about the palette is
;;;; refuse to let structure depend on hue alone, which is what the weight below
;;;; is for: the three heading levels and the modeline are bold, and the
;;;; modeline sits on the Surface ramp rather than on Mantle, so the shape of
;;;; the frame survives in the places where a colour does not carry.
;;;;
;;;; On weight: five faces are bold and two are italic, and that is deliberately
;;;; near the ceiling — a screen where everything is heavy has no emphasis in it
;;;; at all. Catppuccin italicises comments in every port that offers the switch
;;;; (`styles.comments' in the nvim theme, on by default;
;;;; `catppuccin-italic-comments' in the Emacs one, off by default — this file
;;;; takes the italic) and bolds nothing in font-lock at all: keyword, type and
;;;; function are carried by hue alone upstream, and are left that way here
;;;; rather than invented into boldness.
;;;;
;;;; Five mappings are not Catppuccin's own, and they are the same five the
;;;; Mocha file documents, for the same reasons:
;;;;
;;;; * `heading-3' — upstream's `org-level-3' is :weight normal, where levels 1
;;;;   and 2 inherit bold. zemacs paints level 3 and everything below it in the
;;;;   same face, so it is the last rung rather than the middle of a ladder.
;;;; * `modeline' and `modeline-inactive' — upstream puts them on Mantle and
;;;;   Crust, which against Latte's Base are two barely-there greys separated by
;;;;   ten values out of 255; Emacs can tell them apart because it also changes
;;;;   the foreground, and zemacs writes both bars with one `modeline-text'.
;;;;   They take the Surface ramp, where the style guide puts UI chrome anyway,
;;;;   with the active bar one step further from Base than the inactive one.
;;;; * `modeline-text' — bold. Emacs bolds the parts of the mode line that carry
;;;;   the information (`mode-line-buffer-id', `mode-line-emphasis') and leaves
;;;;   the rest plain; with a single face for the whole bar the choice is all or
;;;;   nothing. Here it does double duty: Text on Surface 1 is 4.4:1, which is
;;;;   under the AA threshold for body text and over it for bold.
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
      (base     '(0.937 0.945 0.961))  ; #eff1f5
      (surface1 '(0.737 0.753 0.800))  ; #bcc0cc
      (surface0 '(0.800 0.816 0.855))  ; #ccd0da

      ;; Text, and the overlays that step up from it toward the ground
      (text     '(0.298 0.310 0.412))  ; #4c4f69
      (overlay2 '(0.486 0.498 0.576))  ; #7c7f93
      (overlay1 '(0.549 0.561 0.631))  ; #8c8fa1

      ;; Accents
      (mauve    '(0.533 0.224 0.937))  ; #8839ef
      (red      '(0.824 0.059 0.224))  ; #d20f39
      (peach    '(0.996 0.392 0.043))  ; #fe640b
      (yellow   '(0.875 0.557 0.114))  ; #df8e1d
      (green    '(0.251 0.627 0.169))  ; #40a02b
      (sky      '(0.016 0.647 0.898))  ; #04a5e5
      (blue     '(0.118 0.400 0.961))  ; #1e66f5
      (lavender '(0.447 0.529 0.992))  ; #7287fd
      )

  (apply #'set-background base)
  (apply #'set-foreground text)

  ;; All 22 of them. Anything skipped keeps the last theme's colour and weight.
  (set-face "default"           text)                 ; #4c4f69  `default'
  (set-face "keyword"           mauve)                ; #8839ef  Keywords
  (set-face "function"          blue)                 ; #1e66f5  Methods, Functions
  (set-face "type"              yellow)               ; #df8e1d  Classes, Types
  (set-face "string"            green)                ; #40a02b  Strings
  (set-face "number"            peach)                ; #fe640b  Constants, Numbers
  (set-face "comment"           overlay2 :italic t)   ; #7c7f93  Comments
  (set-face "constant"          peach)                ; #fe640b  Constants, Numbers
  (set-face "variable"          text)                 ; #4c4f69  `font-lock-variable-name-face'
  (set-face "operator"          sky)                  ; #04a5e5  Operators
  (set-face "punctuation"       overlay2)             ; #7c7f93  Braces, Delimiters
  (set-face "heading-1"         red      :bold t)     ; #d20f39  `org-level-1'
  (set-face "heading-2"         peach    :bold t)     ; #fe640b  `org-level-2'
  (set-face "heading-3"         yellow   :bold t)     ; #df8e1d  `org-level-3'
  (set-face "bold"              text     :bold t)     ; #4c4f69  Emacs' own `bold'
  (set-face "italic"            text     :italic t)   ; #4c4f69  Emacs' own `italic'
  (set-face "link"              lavender)             ; #7287fd  `link'
  (set-face "code"              green)                ; #40a02b  `org-code', `org-verbatim'
  (set-face "markup"            overlay1)             ; #8c8fa1  see the header
  (set-face "modeline"          surface1)             ; #bcc0cc  bar, current window
  (set-face "modeline-inactive" surface0)             ; #ccd0da  bar, other windows
  (set-face "modeline-text"     text     :bold t)     ; #4c4f69  what is written on it
  )
