;;;; tokyo-night-storm — Tokyo Night on the blue-grey ground, not the black one
;;;;
;;;; folke's Tokyo Night ships three grounds and one accent set. `night' is
;;;; nearly black, `day' is white, and `storm' — this one — sits on #24283b, a
;;;; desaturated indigo the colour of the city under cloud. The accents are
;;;; character for character the same as plain Tokyo Night; the whole difference
;;;; between the variants is the ground and the greys mixed from it, so those
;;;; are the values worth getting exactly right. Every hex below is the
;;;; published one from `lua/tokyonight/colors/storm.lua'.
;;;;
;;;; Load it from your init.lisp — these are ordinary top-level forms, so
;;;; loading the file *is* applying the theme:
;;;;
;;;;   (load-theme "tokyo-night-storm")
;;;;
;;;; Every face zemacs has is set below, so nothing survives from whatever was
;;;; loaded before. A face left out here is a face the previous theme still owns
;;;; — its colour *and*, now that faces carry weight, its bold and its slant.
;;;; That is the whole reason the list is exhaustive and boring.
;;;;
;;;; Dired and the git status buffer have no faces of their own; they reuse
;;;; these. A directory is `type', an executable `function', a symlink `link',
;;;; a staged file `string' and an unstaged one `keyword'. So the same 22 lines
;;;; that colour a source buffer colour those too.
;;;;
;;;; On weight: Tokyo Night carries emphasis in hue, not in weight, and its own
;;;; face list says so. Upstream bolds two things — `Title', which is every
;;;; heading, and `@markup.strong' — and italicises two, comments and keywords.
;;;; This port bolds and italicises exactly those and nothing else, which is why
;;;; its bold list is shorter than either gruvbox file's. A screen where
;;;; everything is heavy has no emphasis on it at all.
;;;;
;;;; Five mappings are not folke's own:
;;;;
;;;; * `keyword' — Tokyo Night gives two answers. The classic vim `Keyword'
;;;;   group is `cyan'; the treesitter `@keyword' is `purple'. Everything a
;;;;   modern highlighter calls a keyword arrives through `@keyword', so this
;;;;   takes purple, which is also the colour the theme is recognised by.
;;;; * `variable' — `@variable' is plain `fg' upstream, on the theory that a
;;;;   grammar picks out the interesting variables (parameters, fields) one at a
;;;;   time and leaves the rest as text. zemacs has one `variable' face and no
;;;;   such grammar, so it takes the classic `Identifier' group, `magenta'.
;;;; * `punctuation' — `@punctuation.bracket' is `fg_dark' and
;;;;   `@punctuation.delimiter' is `blue5'. In a Lisp buffer this face is almost
;;;;   entirely parentheses, so it follows the bracket, not the delimiter.
;;;; * `markup' — the emphasis marks, bullets and `#+keyword' lines. Upstream
;;;;   paints markdown's structural marks `orange'
;;;;   (`@punctuation.special.markdown'), and this takes that; it drops the
;;;;   bold upstream gives the list bullet alone, because zemacs has no face
;;;;   that narrow and `#+begin_src' does not want to be shouted.
;;;; * `modeline-inactive' — upstream draws both status lines on one background
;;;;   and separates them by foreground. zemacs writes `modeline-text' on both
;;;;   bars, so the separation has to move into the background: the active bar
;;;;   takes `fg_gutter' and the inactive one `bg_statusline', which is the
;;;;   two-tier arrangement the shipped lualine theme uses.

(in-package :zemacs)

;;; The palette, named as folke names it.
(let (;; The ground and the greys mixed from it — the storm variant proper.
      (bg           '(0.141 0.157 0.231))  ; #24283b
      (bg-dark      '(0.122 0.137 0.208))  ; #1f2335  also `bg_statusline'
      (fg-gutter    '(0.231 0.259 0.380))  ; #3b4261
      (fg           '(0.753 0.792 0.961))  ; #c0caf5
      (fg-dark      '(0.663 0.694 0.839))  ; #a9b1d6
      (comment      '(0.337 0.373 0.537))  ; #565f89

      ;; The accents. Shared with `night' and `moon' unchanged: they are chosen
      ;; against `fg', and moving the ground does not move them.
      (blue         '(0.478 0.635 0.969))  ; #7aa2f7
      (blue1        '(0.165 0.765 0.871))  ; #2ac3de
      (blue5        '(0.537 0.867 1.000))  ; #89ddff
      (green        '(0.620 0.808 0.416))  ; #9ece6a
      (magenta      '(0.733 0.604 0.969))  ; #bb9af7
      (orange       '(1.000 0.620 0.392))  ; #ff9e64
      (purple       '(0.616 0.486 0.847))  ; #9d7cd8
      (teal         '(0.102 0.737 0.612))  ; #1abc9c
      (yellow       '(0.878 0.686 0.408))  ; #e0af68
      )

  (apply #'set-background bg)
  (apply #'set-foreground fg)

  ;; All 22 of them. Anything skipped keeps the last theme's colour and weight.
  (set-face "default"           fg)                     ; #c0caf5  `Normal'
  (set-face "keyword"           purple :italic t)       ; #9d7cd8  `@keyword'
  (set-face "function"          blue)                   ; #7aa2f7  `Function'
  (set-face "type"              blue1)                  ; #2ac3de  `Type'
  (set-face "string"            green)                  ; #9ece6a  `String'
  (set-face "number"            orange)                 ; #ff9e64  `Number'
  (set-face "comment"           comment :italic t)      ; #565f89  `Comment'
  (set-face "constant"          orange)                 ; #ff9e64  `Constant'
  (set-face "variable"          magenta)                ; #bb9af7  `Identifier'
  (set-face "operator"          blue5)                  ; #89ddff  `Operator'
  (set-face "punctuation"       fg-dark)                ; #a9b1d6  `@punctuation.bracket'
  (set-face "heading-1"         blue :bold t)           ; #7aa2f7  `rainbow' 1
  (set-face "heading-2"         yellow :bold t)         ; #e0af68  `rainbow' 2
  (set-face "heading-3"         green :bold t)          ; #9ece6a  `rainbow' 3
  (set-face "bold"              fg :bold t)             ; #c0caf5  `Bold'
  (set-face "italic"            fg :italic t)           ; #c0caf5  `Italic'
  (set-face "link"              teal)                   ; #1abc9c  `@markup.link'
  (set-face "code"              green)                  ; #9ece6a  `@markup.raw'
  (set-face "markup"            orange)                 ; #ff9e64  markdown's marks
  (set-face "modeline"          fg-gutter)              ; #3b4261  lualine's raised segment
  (set-face "modeline-inactive" bg-dark)                ; #1f2335  `bg_statusline'
  (set-face "modeline-text"     fg-dark)                ; #a9b1d6  `StatusLine' fg
  )
