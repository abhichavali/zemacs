;;;; modus-vivendi-tinted — Modus Vivendi with the black taken out of it
;;;;
;;;; Same theme, same contrast discipline, warmer ground: the background is a
;;;; deep desaturated indigo (#0d0e1c) rather than pure black, and the modeline
;;;; and accents are tinted to sit on it. Protesilaos ships this for the same
;;;; reason people reach for it — #000000 on a modern panel is a hole in the
;;;; screen, and a near-black with a hue in it reads as a surface.
;;;;
;;;; Every colour is the published `modus-themes-vivendi-tinted-palette' value,
;;;; not an approximation, so the AAA (7:1) contrast the Modus themes exist to
;;;; guarantee survives the port.
;;;;
;;;; Load it from your init.lisp — these are ordinary top-level forms, so
;;;; loading the file *is* applying the theme:
;;;;
;;;;   (load-theme "modus-vivendi-tinted")
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. A face left out here is a face the previous theme still
;;;; owns, which is the whole reason the list is exhaustive and boring.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse
;;;; these. A directory is `type', an executable `function', a symlink `link',
;;;; a staged file `string' and an unstaged one `keyword'. So the same lines
;;;; that colour a source buffer colour those too.
;;;;
;;;; The mappings that are not Modus' own are the same ones the plain vivendi
;;;; file documents, and for the same reasons: `number' borrows `identifier',
;;;; the three `heading-N' take Modus' coloured levels 0/2/3 *and* the bold
;;;; weight Modus puts on its headings, and `bold'/`italic' take
;;;; `fg-prose-verbatim'/`docstring' on top of the real weight and slant they
;;;; now carry.
;;;;
;;;; Nothing else in this file is bold. Modus' own `modus-themes-bold-constructs'
;;;; and `modus-themes-italic-constructs' both default to nil — keywords are not
;;;; heavy and comments are not slanted in Modus, and a port that reached for the
;;;; new weight everywhere would stop being this theme.
;;;;
;;;; What you actually see may still be heavier, because `*bold-constructs*' in
;;;; init.lisp is applied on top of whichever theme is loaded and bolds
;;;; keywords, functions and types by default. That is zemacs' taste rather than
;;;; Modus', and it is a variable for exactly that reason — set it to NIL for
;;;; this theme as Protesilaos published it.

(in-package :zemacs)

;;; The palette, named as Modus names it.
(let (;; Basic values
      (bg-main               '(0.051 0.055 0.110))  ; #0d0e1c
      (fg-main               '(1.000 1.000 1.000))  ; #ffffff
      (fg-dim                '(0.616 0.616 0.667))  ; #9d9d9daa -> #9d9d9d

      ;; Accent foregrounds. Identical to plain vivendi: the accents are chosen
      ;; against `fg-main', and the ground moving does not move them.
      (blue-warmer           '(0.475 0.659 1.000))  ; #79a8ff
      (blue-cooler           '(0.000 0.737 1.000))  ; #00bcff
      (blue-faint            '(0.510 0.690 0.925))  ; #82b0ec
      (cyan                  '(0.000 0.827 0.816))  ; #00d3d0
      (cyan-cooler           '(0.416 0.894 0.725))  ; #6ae4b9
      (green-faint           '(0.533 0.792 0.624))  ; #88ca9f
      (magenta               '(0.996 0.675 0.816))  ; #feacd0
      (magenta-warmer        '(0.969 0.561 0.906))  ; #f78fe7
      (magenta-cooler        '(0.714 0.627 1.000))  ; #b6a0ff
      (yellow-faint          '(0.824 0.710 0.502))  ; #d2b580

      ;; Modeline backgrounds — tinted, which is the whole point of the variant.
      (bg-mode-line-active   '(0.267 0.243 0.376))  ; #443f60
      (bg-mode-line-inactive '(0.157 0.157 0.208))  ; #282835
      )

  (apply #'set-background bg-main)
  (apply #'set-foreground fg-main)

  ;; All of them. Anything skipped keeps the last theme's colour *and weight*,
  ;; which is the half that bites: an unset face inheriting a bold from the
  ;; theme before it looks like a highlighter bug rather than a missing line.
  (flet ((face (name rgb &key bold italic)
           (set-face name rgb :bold bold :italic italic)))
    (face "default"           fg-main)                ; #ffffff  Modus `fg-main'
    (face "keyword"           magenta-cooler)         ; #b6a0ff  Modus `keyword'
    (face "function"          magenta)                ; #feacd0  Modus `fnname'
    (face "type"              cyan-cooler)            ; #6ae4b9  Modus `type'
    (face "string"            blue-warmer)            ; #79a8ff  Modus `string'
    (face "number"            yellow-faint)           ; #d2b580  Modus `identifier'
    (face "comment"           fg-dim)                 ; #9d9d9d  Modus `comment'
    (face "constant"          blue-cooler)            ; #00bcff  Modus `constant'
    (face "variable"          cyan)                   ; #00d3d0  Modus `variable'
    (face "operator"          fg-main)                ; #ffffff  Modus `operator'
    (face "punctuation"       fg-main)                ; #ffffff  Modus `punctuation'
    (face "heading-1"         cyan-cooler   :bold t)  ; #6ae4b9  Modus `fg-heading-0'
    (face "heading-2"         yellow-faint  :bold t)  ; #d2b580  Modus `fg-heading-2'
    (face "heading-3"         blue-faint    :bold t)  ; #82b0ec  Modus `fg-heading-3'
    (face "bold"              magenta-warmer :bold t) ; #f78fe7  Modus `fg-prose-verbatim'
    (face "italic"            green-faint :italic t)  ; #88ca9f  Modus `docstring'
    (face "link"              blue-warmer)            ; #79a8ff  Modus `fg-link'
    (face "code"              cyan-cooler)            ; #6ae4b9  Modus `fg-prose-code'
    (face "markup"            fg-dim)                 ; #9d9d9d  Modus `prose-metadata'
    (face "modeline"          bg-mode-line-active)    ; #443f60  Modus `bg-mode-line-active'
    (face "modeline-inactive" bg-mode-line-inactive)  ; #282835  Modus `bg-mode-line-inactive'
    (face "modeline-text"     fg-main)                ; #ffffff  Modus `fg-mode-line-active'
    ))
