;;;; The modes zemacs ships, defined with the machinery in `modes.lisp'.
;;;;
;;;; Loaded by that file; there is nothing to call. Every mode the editor can
;;;; arrive at on its own is here — the editor names a major mode after the
;;;; language it detected (`rust' -> `rust-mode') and falls back to
;;;; `fundamental-mode', and a mode that is *not* defined here is one this image
;;;; is never told about, so no setting it claims would ever be released. Adding
;;;; a language to the highlighter therefore means adding a line below.
;;;;
;;;; Notice what is not here: no mode has an entry body. Settings and keys are
;;;; declared next to the definition, once, rather than being redone on every
;;;; entry — the body is for a mode that has real work to do, and none of these
;;;; do yet.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; The roots
;;;
;;; `fundamental-mode' is what the editor uses for a file it has no grammar
;;; for. It claims nothing: it *is* the baseline, and the whole point of the
;;; mode-local machinery is that it no longer has to undo other modes' settings
;;; by hand.

(define-derived-mode fundamental-mode nil)

;;; `prog-mode' is never entered directly — it exists to be derived from, the
;;; way it does in Emacs, so that everything true of code is said once.
(define-derived-mode prog-mode nil)

;;; Code does not wrap, even where the global default does: a wrapped line
;;; hides the indentation that shows the structure.
(set-mode-local 'prog-mode 'line-overflow "truncate")

;;; Inherited by every mode below that derives from `prog-mode', because
;;; `define-mode-key' re-issues it under each child's name — the editor's mode
;;; keymap is keyed by the exact mode name and knows nothing about inheritance.
(define-mode-key 'prog-mode "SPC t w" "visual-line")

;;; ---------------------------------------------------------------------------
;;; Prose

(define-derived-mode text-mode nil)
(set-mode-local 'text-mode 'line-overflow "wrap")

;;; Org is prose with structure, and takes the wrapping from `text-mode'.
;;;
;;; It used to claim `relative-line-numbers' off as well, on the theory that
;;; relative numbering is a motion aid for `5j'-style editing and prose is read
;;; rather than jumped through. That theory is wrong about how org is actually
;;; edited — an outline is exactly the thing you move through in counted jumps
;;; — and the claim had a worse effect than being wrong: because a mode claim
;;; beats the global baseline, turning relative numbers on for the editor did
;;; nothing at all in the buffers where they were wanted. A setting nobody
;;; claims is a setting the config decides, which is the right default here.
(define-derived-mode org-mode text-mode)

;;; No gutter — but said in `init.lisp' with `set-no-gutter-modes', not here
;;; with `set-mode-local'.
;;;
;;; The claim used to be here and it was subtly wrong in the same way the
;;; `relative-line-numbers' one above was, only harder to see. A mode-local
;;; setting is *global*: entering org applies it to the editor, so a source file
;;; in the pane alongside lost its numbers too, and which of the two won came
;;; down to which mode had been entered last. Line numbers are drawn per pane,
;;; so they have to be decided per buffer, which is a mechanism this table does
;;; not have and deliberately is not growing — `set-no-gutter-modes' is a list
;;; of mode names that core applies to each buffer as its mode is set.
;;;
;;; A measure, on the other hand, genuinely is a claim: 80 columns rather than a
;;; line as wide as the window. 80 is
;;; the number this file already assumes everywhere else, and it lands inside
;;; the 45-90 characters typography has settled on for a comfortable line: past
;;; that the eye loses the start of the next line on the way back to it. The
;;; renderer centres whatever it is given, so a wide window becomes margin
;;; rather than a longer line — `olivetti-mode', and what a document looks like.
;;;
;;; A number and not a fraction of the window on purpose: a measure is a count
;;; of characters, so it stays right when the window is resized and when the
;;; font size changes, and it means the same thing on a laptop as on a 5K panel.
(set-mode-local 'org-mode 'text-width 80)

;;; ---------------------------------------------------------------------------
;;; Languages
;;;
;;; One per language the highlighter knows, since the editor will name these
;;; modes whether or not anything defines them.

(define-derived-mode rust-mode prog-mode)
(define-derived-mode lisp-mode prog-mode)
(define-derived-mode python-mode prog-mode)
(define-derived-mode c-mode prog-mode)
(define-derived-mode javascript-mode prog-mode)
(define-derived-mode json-mode prog-mode)
(define-derived-mode toml-mode prog-mode)

;;; Lisp indents by two, and `eval-dwim' — the built-in verb behind `C-c' —
;;; earns a mode-local spelling in the buffers where it means something.
(set-mode-local 'lisp-mode 'tab-width 2)
;;; `lisp-eval-dwim' rather than the built-in `eval-dwim' verb: same three-way
;;; decision, but the value goes to the transcript. Defined in `repl.lisp',
;;; which loads after this — a binding names a command by string and is looked
;;; up when the key is pressed, so the order does not matter.
(define-mode-key 'lisp-mode "SPC m e" "lisp-eval-dwim")

;;; ---------------------------------------------------------------------------
;;; Minor modes
;;;
;;; `visual-line' declares a setting and nothing else, which is what most minor
;;; modes look like here: the enable/disable bodies exist for the ones that have
;;; work to do, and claiming a setting is not work — it is a fact.

(define-minor-mode visual-line
  "Wrap long lines in this buffer instead of truncating them.")

(set-mode-local 'visual-line 'line-overflow "wrap")

;;; `SPC t w' reaches it in every code buffer, through `prog-mode' above. No
;;; global binding here: this file must load whether or not the init file that
;;; loads it has defined `define-leader' yet, so it only ever uses primitives.

;;; ---------------------------------------------------------------------------
;;; Filenames the editor has no language for
;;;
;;; `.org' and the seven languages above already arrive with a mode, because
;;; the editor recognises their extensions itself. These are the ones that
;;; would otherwise sit in `fundamental-mode' — see `*auto-mode-alist*', which
;;; already lists README, LICENSE, CHANGELOG, .txt, .text, .md and .markdown.

(add-auto-mode ".rst" 'text-mode)
(add-auto-mode ".adoc" 'text-mode)
