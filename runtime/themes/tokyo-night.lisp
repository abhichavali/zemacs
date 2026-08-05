;;;; tokyo-night — folke's night lights, the darkest of the Tokyo Night variants
;;;;
;;;; Tokyo Night began as a VS Code theme and reached everybody else through
;;;; `folke/tokyonight.nvim', which is where the palette below comes from: the
;;;; `night' variant is the shared `storm' palette with the ground dropped to
;;;; #1a1b26. The values are the published ones, not approximations, and the
;;;; assignments follow folke's own highlight groups and bbatsov's Emacs port
;;;; `tokyo-night-emacs', which agree almost everywhere — keywords Magenta,
;;;; functions Blue, types Cyan, strings Green, constants and numbers Orange,
;;;; operators the pale cyan folke calls `blue5'.
;;;;
;;;; folke's names use underscores; they are spelled with hyphens here and are
;;;; otherwise his.
;;;;
;;;; Load it from your init.lisp — these are ordinary top-level forms, so
;;;; loading the file *is* applying the theme:
;;;;
;;;;   (load-theme "tokyo-night")
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
;;;; On weight: five faces are bold and three are italic. Tokyo Night is the
;;;; more opinionated of the two themes shipped here about slant — both
;;;; `tokyonight.nvim' and the Emacs port italicise comments *and* keywords by
;;;; default (`styles.keywords', `tokyo-night-italic-keywords'), and that is
;;;; kept, because it is most of what the theme looks like in the wild. Neither
;;;; upstream bolds anything in font-lock, so keyword, type and function are
;;;; carried by hue here as they are there. The bold goes to structure: folke
;;;; sets every markdown heading `bold = true' and bbatsov every `outline-N'
;;;; `:weight bold', and the modeline joins them for the reason below.
;;;;
;;;; Four mappings are not Tokyo Night's own:
;;;;
;;;; * `heading-1' through `heading-3' — folke has two answers and they disagree.
;;;;   His markdown headings walk a rainbow whose first three entries are Blue,
;;;;   Yellow and Green; bbatsov's `outline-1..3' walk a blue ramp, #89ddff to
;;;;   #61bdf2 to #7aa2f7, of which the middle value is the port's own and is
;;;;   not in folke's palette at all. zemacs has exactly three heading faces
;;;;   rather than eight, and three near-identical blues in a row would name
;;;;   nothing, so the rainbow wins and the ramp is dropped.
;;;; * `modeline' and `modeline-inactive' — folke draws both statuslines on
;;;;   `bg_dark' and tells them apart by foreground, `fg_dark' against
;;;;   `fg_gutter'. zemacs writes both bars with one `modeline-text', so the
;;;;   difference has to live in the bar: `bg_highlight' raises the focused one
;;;;   off the buffer and `bg_dark' sinks the rest into it, which is the same
;;;;   thing `set-modeline-relief' is drawing in bevel.
;;;; * `modeline-text' — bold, and folke's `fg' rather than the dimmer
;;;;   `fg_sidebar' he uses for the bar. Emacs bolds the parts of the mode line
;;;;   that carry the information (`mode-line-buffer-id', `mode-line-emphasis')
;;;;   and leaves the rest plain; with a single face for the whole bar the
;;;;   choice is all or nothing.
;;;; * `code' and `link' — upstream splits `=verbatim=' from `~code~' and zemacs
;;;;   does not: bbatsov gives `org-verbatim' Green and `org-code' the bright
;;;;   teal his port calls `teal' and folke calls `green1'. The one face takes
;;;;   `green1', because Green is already `string' and, more to the point,
;;;;   because `link' has to keep folke's `teal' (#1abc9c, his `@markup.link')
;;;;   and the two would have collided the other way round.
;;;;
;;;; `variable' is `fg', which is not a deviation but is worth saying out loud:
;;;; both upstreams leave a plain identifier in the body colour and paint only
;;;; the ones that are something in particular — a property, a parameter, a
;;;; builtin. The theme is vivid because of what it marks, not because it marks
;;;; everything.

(in-package :zemacs)

;;; The palette, named as folke names it.
(let (;; Grounds
      (bg           '(0.102 0.106 0.149))  ; #1a1b26
      (bg-dark      '(0.086 0.086 0.118))  ; #16161e
      (bg-highlight '(0.161 0.180 0.259))  ; #292e42

      ;; Foregrounds
      (fg           '(0.753 0.792 0.961))  ; #c0caf5
      (fg-dark      '(0.663 0.694 0.839))  ; #a9b1d6
      (comment      '(0.337 0.373 0.537))  ; #565f89

      ;; Accents
      (blue         '(0.478 0.635 0.969))  ; #7aa2f7
      (blue5        '(0.537 0.867 1.000))  ; #89ddff
      (cyan         '(0.490 0.812 1.000))  ; #7dcfff
      (magenta      '(0.733 0.604 0.969))  ; #bb9af7
      (orange       '(1.000 0.620 0.392))  ; #ff9e64
      (yellow       '(0.878 0.686 0.408))  ; #e0af68
      (green        '(0.620 0.808 0.416))  ; #9ece6a
      (green1       '(0.451 0.855 0.792))  ; #73daca  (bbatsov's port calls this `teal')
      (teal         '(0.102 0.737 0.612))  ; #1abc9c
      )

  (apply #'set-background bg)
  (apply #'set-foreground fg)

  ;; All 22 of them. Anything skipped keeps the last theme's colour and weight.
  (set-face "default"           fg)                    ; #c0caf5  `default'
  (set-face "keyword"           magenta  :italic t)    ; #bb9af7  `font-lock-keyword-face'
  (set-face "function"          blue)                  ; #7aa2f7  `font-lock-function-name-face'
  (set-face "type"              cyan)                  ; #7dcfff  `font-lock-type-face'
  (set-face "string"            green)                 ; #9ece6a  `String'
  (set-face "number"            orange)                ; #ff9e64  `Number'
  (set-face "comment"           comment  :italic t)    ; #565f89  `Comment'
  (set-face "constant"          orange)                ; #ff9e64  `Constant'
  (set-face "variable"          fg)                    ; #c0caf5  `@variable'
  (set-face "operator"          blue5)                 ; #89ddff  `@operator'
  (set-face "punctuation"       fg-dark)               ; #a9b1d6  `font-lock-delimiter-face'
  (set-face "heading-1"         blue     :bold t)      ; #7aa2f7  `@markup.heading.1'
  (set-face "heading-2"         yellow   :bold t)      ; #e0af68  `@markup.heading.2'
  (set-face "heading-3"         green    :bold t)      ; #9ece6a  `@markup.heading.3'
  (set-face "bold"              fg       :bold t)      ; #c0caf5  folke's `Bold'
  (set-face "italic"            fg       :italic t)    ; #c0caf5  folke's `Italic'
  (set-face "link"              teal)                  ; #1abc9c  `@markup.link'
  (set-face "code"              green1)                ; #73daca  `org-code'
  (set-face "markup"            comment)               ; #565f89  `org-meta-line'
  (set-face "modeline"          bg-highlight)          ; #292e42  bar, current window
  (set-face "modeline-inactive" bg-dark)               ; #16161e  bar, other windows
  (set-face "modeline-text"     fg       :bold t)      ; #c0caf5  what is written on it
  )
