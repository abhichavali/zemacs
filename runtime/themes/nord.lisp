;;;; nord — an arctic, north-bluish palette, and the quiet one in this set
;;;;
;;;; Arctic Ice Studio's Nord, ported from `nord-theme.el'. Nord is not a
;;;; sixteen-colour dark theme that happens to be blue; it is four named groups
;;;; — Polar Night (nord0–nord3, the grounds), Snow Storm (nord4–nord6, the
;;;; foregrounds), Frost (nord7–nord10, the cool accents) and Aurora
;;;; (nord11–nord15, the warm ones) — and the port keeps the numbering so every
;;;; line below can be checked against the published spec without translation.
;;;;
;;;;   (load-theme "nord")
;;;;
;;;; This one is deliberately not vibrant. Nord's whole argument is a narrow
;;;; hue range and low saturation, and upstream uses weight about as sparingly:
;;;; `nord-theme.el' bolds exactly one font-lock face (the preprocessor, which
;;;; zemacs has no equivalent for), leaves comments upright, and reserves its
;;;; emphasis for org headings. That restraint is ported rather than corrected.
;;;; If you want a screen that shouts, one of the other themes in this directory
;;;; is the one you want; this is the one for a long afternoon.
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. A face left out here is a face the previous theme still
;;;; owns — its colour *and now its weight*, which is the worse half — and that
;;;; is the whole reason the list is exhaustive and boring. It also matters more
;;;; for this theme than for any other: load Nord after a loud theme, miss a
;;;; face, and the one thing left bold is the thing Nord was never going to
;;;; emphasise.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse
;;;; these. A directory is `type', an executable `function', a symlink `link',
;;;; a staged file `string' and an unstaged one `keyword'. So the same lines
;;;; that colour a source buffer colour those too.
;;;;
;;;; Repeated colours below are upstream's doing, not an oversight: `keyword',
;;;; `constant' and `operator' are all nord9, and `default', `variable' and
;;;; `punctuation' are all nord4, because `nord-keyword', `nord-operator' and
;;;; `nord-punctuation' resolve that way in `nord-theme.el'.
;;;;
;;;; Four mappings are not upstream's own:
;;;;
;;;; * `comment' — the numbered palette has no comment colour. Upstream once
;;;;   computed one by brightening nord3 and, since 0.4.0, simply ships the
;;;;   brightened value: #616e88. It is bound below as `nord-comment' next to
;;;;   the numbers so the deviation is visible rather than smuggled in. Plain
;;;;   nord3 on nord0 is under 2:1 and is not a colour to read prose in.
;;;; * `markup' — upstream paints org's metadata keywords in nord3, which has
;;;;   the same problem for the same reason. zemacs draws markup on the buffer
;;;;   ground, so it takes the brightened comment colour instead — the answer
;;;;   Nord itself reached for when it hit this.
;;;; * `heading-1/2/3' — upstream grades them extra-bold, bold, semi-bold.
;;;;   zemacs has one bold, so all three take it and the level is carried by
;;;;   hue, which is what upstream is doing anyway: nord7, nord8, nord9 walking
;;;;   down Frost.
;;;; * `link' — Nord's `link' face is underline with no colour at all, and
;;;;   zemacs does not underline. It takes nord8, which is the colour Nord uses
;;;;   for `button', `org-date' and `org-footnote', i.e. for the things that are
;;;;   somewhere you can go.

(in-package :zemacs)

;;; The palette, numbered as the Nord spec numbers it.
(let (;; Polar Night — the grounds
      (nord0        '(0.180 0.204 0.251))  ; #2e3440
      (nord1        '(0.231 0.259 0.322))  ; #3b4252
      (nord2        '(0.263 0.298 0.369))  ; #434c5e
      (nord3        '(0.298 0.337 0.416))  ; #4c566a

      ;; Snow Storm — the foregrounds
      (nord4        '(0.847 0.871 0.914))  ; #d8dee9
      (nord5        '(0.898 0.914 0.941))  ; #e5e9f0
      (nord6        '(0.925 0.937 0.957))  ; #eceff4

      ;; Frost — the cool accents
      (nord7        '(0.561 0.737 0.733))  ; #8fbcbb
      (nord8        '(0.533 0.753 0.816))  ; #88c0d0
      (nord9        '(0.506 0.631 0.757))  ; #81a1c1
      (nord10       '(0.369 0.506 0.675))  ; #5e81ac

      ;; Aurora — the warm ones
      (nord11       '(0.749 0.380 0.416))  ; #bf616a
      (nord12       '(0.816 0.529 0.439))  ; #d08770
      (nord13       '(0.922 0.796 0.545))  ; #ebcb8b
      (nord14       '(0.639 0.745 0.549))  ; #a3be8c
      (nord15       '(0.706 0.557 0.678))  ; #b48ead

      ;; Not a numbered colour: `nord-theme.el' brightens nord3 by ten percent
      ;; for comments, because nord3 on nord0 is not legible enough to read.
      (nord-comment '(0.380 0.431 0.533))  ; #616e88
      )
  ;; Nord numbers sixteen colours and zemacs spends nine of them. The rest are
  ;; bound anyway so the palette above can be read straight down against the
  ;; spec; a list with gaps in it is a list nobody can check. Most of what goes
  ;; unspent is diagnostics — nord11 error, nord13 warning, nord12 deprecation —
  ;; which zemacs has no faces for yet.
  (declare (ignorable nord2 nord5 nord6 nord10 nord11 nord12 nord13))

  (apply #'set-background nord0)
  (apply #'set-foreground nord4)

  ;; All 22 of them. Anything skipped keeps the last theme's colour and weight.
  ;; The trailing name is the `nord-theme.el' face the mapping came from, with
  ;; the `font-lock-' prefix and the `-face' suffix left off.
  (set-face "default"           nord4)             ; #d8dee9  default
  (set-face "keyword"           nord9)             ; #81a1c1  keyword
  (set-face "function"          nord8)             ; #88c0d0  function-name
  (set-face "type"              nord7)             ; #8fbcbb  type
  (set-face "string"            nord14)            ; #a3be8c  string
  (set-face "number"            nord15)            ; #b48ead  nord-numeric
  (set-face "comment"           nord-comment)      ; #616e88  comment, upright
  (set-face "constant"          nord9)             ; #81a1c1  constant
  (set-face "variable"          nord4)             ; #d8dee9  variable-name
  (set-face "operator"          nord9)             ; #81a1c1  nord-operator
  (set-face "punctuation"       nord4)             ; #d8dee9  nord-punctuation
  (set-face "heading-1"         nord7 :bold t)     ; #8fbcbb  org-level-1
  (set-face "heading-2"         nord8 :bold t)     ; #88c0d0  org-level-2
  (set-face "heading-3"         nord9 :bold t)     ; #81a1c1  org-level-3
  (set-face "bold"              nord4 :bold t)     ; #d8dee9  bold, weight only
  (set-face "italic"            nord4 :italic t)   ; #d8dee9  italic, slant only
  (set-face "link"              nord8)             ; #88c0d0  see the header
  (set-face "code"              nord7)             ; #8fbcbb  org-code
  (set-face "markup"            nord-comment)      ; #616e88  see the header
  (set-face "modeline"          nord3)             ; #4c566a  mode-line background
  (set-face "modeline-inactive" nord1)             ; #3b4252  mode-line-inactive bg
  (set-face "modeline-text"     nord8)             ; #88c0d0  mode-line foreground
  )
