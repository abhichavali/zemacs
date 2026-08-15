;;; zemacs configuration — Common Lisp.
;;;;
;;;; This is *your* file. It lives in `~/.zemacs.d/init.lisp'; the copy in the
;;;; source tree is the default it was seeded from, and the editor stops reading
;;;; that one the moment yours exists. Edit it with `C-c i', reload it with
;;;; `SPC h r', and nothing you write here can be overwritten by an upgrade.
;;;;
;;;; What is *not* here is the machinery: `set-face', `define-leader',
;;;; `load-theme', `project-make', `refresh-commands' and the rest live in
;;;; `library.lisp' beside the editor, which is upgraded with it. So this file
;;;; stays a list of decisions — which theme, which keys, which modes — and the
;;;; implementation under those decisions goes on improving without you having to
;;;; merge anything. The rule for which file a thing belongs in: if changing it
;;;; would change *your* editor it is config, and if changing it would change
;;;; *every* zemacs it is library.
;;;;
;;;; It is LOADed on startup by the embedded ECL image, which runs on its own
;;;; thread with its own GC. Everything below is ordinary Common Lisp, evaluated
;;;; at startup, and anything you can express in CL you can express here.
;;;;
;;;; If a form in this file signals an error, the error is caught and shown in
;;;; the status line — the editor still starts, it just stops loading here.
;;;;
;;;; Host primitives live in the ZEMACS package.
;;;;
;;;;   (set-font-size n)                       point size
;;;;   (set-background r g b)                  components 0.0 .. 1.0
;;;;   (set-foreground r g b)
;;;;   (set-syntax-color "keyword" r g b)      keyword function type string
;;;;                                           number comment constant variable
;;;;                                           operator punctuation default
;;;;                                           modeline modeline-inactive
;;;;                                           modeline-text
;;;;   (set-line-numbers t)                    or NIL — the editor-wide default
;;;;   (set-no-gutter-modes '("org-mode"))     the modes that overrule it, per
;;;;                                           buffer; replaces the whole list
;;;;   (set-tab-width n)
;;;;   (set-text-width n)                      columns, centred; 0 is off
;;;;   (set-modeline-relief n)                 bevel px; negative sinks it
;;;;   (set-modeline-pad n)                    padding px inside the modeline
;;;;   (set-completion-style "center")         "minibuffer" "bottom" "center"
;;;;   (clear-commands) (register-command "name")   what M-x offers
;;;;   (message text)                          status line
;;;;   (insert text)                           into the current buffer
;;;;   (find-file "path") (save-file) (save-file "path")
;;;;   (show-dashboard) (quit)
;;;;   (dashboard-banner text)
;;;;   (clear-dashboard-items)
;;;;   (dashboard-item #\f "Find file" "find-file")
;;;;   (define-key "normal" "SPC f f" "find-file")
;;;;
;;;; Readers. These answer from the live editor, so a command can depend on
;;;; where the cursor is and what is selected. All take no arguments except
;;;; `buffer-substring'; offsets are characters, counted from 0.
;;;;
;;;;   (point) (point-min) (point-max) (buffer-size)
;;;;   (line-number) (column)                  1-based line, 0-based column
;;;;   (line-count) (line-start) (line-end)
;;;;   (buffer-string) (line-string) (buffer-substring beg end)
;;;;   (buffer-name) (buffer-file-name)        the latter NIL for a scratch buffer
;;;;   (buffer-modified-p) (buffer-read-only-p) (buffer-list)
;;;;   (major-mode) (minor-modes) (evil-state) "normal" "insert" "visual" ...
;;;;   (region)                                (BEG . END), or NIL if nothing is
;;;;                                           selected
;;;;   (region-beginning) (region-end) (region-text)
;;;;   (region-ranges)                         one per line in visual block mode
;;;;   (window-scroll) (window-height) (frame-count)
;;;;
;;;; Writers, beyond `insert' above:
;;;;
;;;;   (goto-char n)
;;;;   (delete-region beg end)
;;;;   (replace-region beg end text)           atomic; see `surround-region'
;;;;   (set-evil-state "normal")
;;;;
;;;; The full reference — the library's own functions included — is in
;;;; `docs/reference.org', and `SPC h t' is the tutorial.
;;;;
;;;; Lisp runs on its own thread and never blocks redisplay or your typing —
;;;; unlike Emacs, where a slow function freezes the editor until it returns. The
;;;; cost is that a *sequence* of commands is not atomic: a keystroke can land
;;;; between two of them. Where that matters, use the one primitive that does the
;;;; whole job (`replace-region') rather than several that each do part of it.
;;;;
;;;; The exception is `find-file', `save-file', git and dired: those need the
;;;; application rather than the editor core, so they take effect a moment later
;;;; and a reader called immediately afterwards still sees the old buffer.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; The library
;;;
;;; `*runtime-dir*' is where the editor keeps its shipped Lisp — themes/, modes/,
;;; lsp.lisp and `library.lisp' itself. The editor sets it before this file is
;;; read, out of `$ZEMACS_RUNTIME'; the fallback below is what makes a config
;;; that is *still* the copy in the source tree work when it is loaded by hand,
;;; which is how the test suite reads it.
;;;
;;; It is deliberately not derived from where *this* file lives. A config in
;;; `~/.zemacs.d/' has no themes/ directory beside it, and looking for one there
;;; is the bug this variable exists to not have.

(unless (boundp '*runtime-dir*)
  (defparameter *runtime-dir* nil))

(unless *runtime-dir*
  (setf *runtime-dir*
        (and *load-truename*
             (make-pathname :name nil :type nil :defaults *load-truename*))))

;;; An error rather than a message, and it is the one place in this file that
;;; deserves one: with no runtime there is no `set-face', no `define-leader' and
;;; no modes, so every form below would fail in turn and the status line would
;;; report whichever failed first. Stopping here names the actual cause.
(unless *runtime-dir*
  (error "zemacs: cannot find the runtime — set $ZEMACS_RUNTIME to the ~
          directory holding library.lisp"))

(load (merge-pathnames "library.lisp" *runtime-dir*) :verbose nil :print nil)

;;; LOAD binds *LOAD-TRUENAME* while this file is being read — and rebinds it
;;; around the nested load above, which is why this comes after rather than
;;; before. `reload-config' and `edit-config' are the two readers of it.
(setf *config-file* *load-truename*)

;;; ---------------------------------------------------------------------------
;;; Appearance

;;; `*font-size*' is what `text-scale-increase' steps from, so it is set beside
;;; the primitive rather than left at the library's default — `set-scale' does
;;; both, but it also announces itself in the status line, which is not what a
;;; boot wants.
(setf *font-size* 16)
(set-font-size *font-size*)
(set-line-numbers t)

;;; ...and then the exceptions, buffer by buffer.
;;;
;;; `set-line-numbers' above is the editor-wide default and every buffer follows
;;; it; this names the major modes that do not. The distinction matters because
;;; the gutter is drawn per *pane*: with an org file open beside a source file,
;;; one editor-wide flag can only give them the same answer. It used to be said
;;; with `set-mode-local', and that is exactly what went wrong — a mode-local
;;; setting is global by construction, so entering org turned the numbers off in
;;; the code buffer next to it, and which pane won depended on which mode had
;;; been entered last.
;;;
;;; Prose is the case worth stating: a line number is a coordinate for talking
;;; to a compiler or another person about code, and nobody has ever cited a line
;;; of prose by number. Terminals and the dashboard are *not* here — they have no
;;; buffer lines to number and the renderer knows it, so they are not a decision
;;; anyone should have to remember to write down.
(set-no-gutter-modes '("org-mode" "org-frozen-mode" "text-mode" "tutor-mode"))
;; t counts from the cursor, vim-style — and counts *visual* lines, so with
;; wrapping on a long paragraph is numbered once per row. That is Emacs'
;; `display-line-numbers-type 'visual', and it is the reading that agrees with
;; `j' and `k': `3j' lands on the row labelled 3.
(set-relative-line-numbers t)
(set-tab-width 4)

;;; Full-width text everywhere by default: code wants every column it can get,
;;; and indentation read down the middle of a pane is indentation you have to
;;; hunt for. `org-mode' claims this for itself in `modes/library.lisp' — a
;;; measure is a fact about *prose*, not about the editor.
(set-text-width 0)

;;; What a window does with a line wider than it is.
;;; "wrap"     — continue it on the next row
;;; "truncate" — cut it at the pane edge and mark the tail with a `→'
;;;
;;; Wrapping, because the alternative hides text and says so with one arrow:
;;; you learn that a line is long without learning what is on it, and the only
;;; way to read the rest is to scroll sideways. `j' and `k' move by *visual*
;;; line when this is on, so a wrapped line still steps a row at a time.
(set-line-overflow "wrap")

;;; A calm dark theme: a near-black blue-grey ground, cool off-white text.
(set-background 0.07 0.08 0.12)
(set-foreground 0.86 0.90 1.00)

;;; Syntax faces. Any face you leave out keeps its built-in colour.
(set-syntax-color "keyword"  0.78 0.57 0.94)
(set-syntax-color "function" 0.51 0.75 1.00)
(set-syntax-color "type"     0.45 0.86 0.83)
(set-syntax-color "string"   0.62 0.85 0.55)
(set-syntax-color "number"   0.98 0.72 0.47)
(set-syntax-color "comment"  0.42 0.46 0.58)

;;; Markup faces, used by org-mode.
;;;
;;; Emphasis used to be carried by colour *instead of* weight and slant, because
;;; the renderer opened one font face. It opens several now, so `*bold*' is bold
;;; and `/italic/' is italic — and these colours changed with that, because they
;;; were doing two jobs and now do one. A tint that had to be distinguishable
;;; from body text all by itself could not also be subtle; freed of that, it can
;;; be bright enough to read as deliberate.
;;;
;;; Brighter here means *further from the background*, not further from white:
;;; the ground is a near-black blue-grey, so these are pushed up in value and
;;; kept saturated, which is what makes them sing against it rather than wash
;;; out. Three levels and no more: the highlighter paints level 3 and everything
;;; below it in the same face, so a `heading-4' here would be a line that names
;;; nothing. Depth past three is carried by the bullet and the indent.
(set-syntax-color "heading-1" 0.62 0.84 1.00) ; bright azure
(set-syntax-color "heading-2" 0.52 0.95 0.84) ; bright aqua
(set-syntax-color "heading-3" 0.86 0.78 1.00) ; bright lilac
(set-syntax-color "bold"      1.00 0.95 0.78) ; near-white gold: heaviest thing here
(set-syntax-color "italic"    0.80 0.94 0.74)
(set-syntax-color "code"      0.70 0.90 1.00)
(set-syntax-color "link"      0.55 0.82 1.00)
(set-syntax-color "markup"    0.36 0.40 0.52) ; the delimiters themselves, dim on purpose

;;; The modeline. Relief is Emacs' `:box :line-width': the magnitude is the
;;; bevel in pixels and the *sign* picks which way it goes — 2 raises the bar
;;; off the buffer, -2 sinks it into the window. 0 is flat.
(set-modeline-relief 2)
(set-modeline-pad 8)

;;; Its faces go through `set-syntax-color' like any other: the modeline lives
;;; in the same colour table, so there is nothing new to learn.
(set-syntax-color "modeline"          0.16 0.18 0.26) ; bar, current window
(set-syntax-color "modeline-inactive" 0.10 0.11 0.17) ; bar, other windows
(set-syntax-color "modeline-text"     0.80 0.85 0.97) ; what is written on it

;;; Everything above is the fallback palette. Loading a theme replaces all of
;;; it — colour *and* weight — so comment this line out to keep the defaults,
;;; put any name from the themes/ directory here, or pick one at runtime with
;;; `M-x theme' / `SPC t t'.
;;;
;;; Tokyo Night rather than Modus, which was the default for as long as Modus
;;; was the only dark theme here. Modus is the better-engineered of the two and
;;; is still one keystroke away: every colour in it clears WCAG AAA against its
;;; ground, which is a real property and not a claim most themes can make. It is
;;; also, by design, quiet — restraint is the thing it is *for*. Tokyo Night is
;;; not trying to be restrained, and a first boot should show what the editor
;;; can do rather than how little it is willing to do.
;;;
;;; `*bold-constructs*' is the taste applied on top of whichever theme loads:
;;; `(setf *bold-constructs* nil)' above this line gives you the ports exactly
;;; as their authors published them.
(load-theme "tokyo-night")

;;; Where completing prompts (M-x, find-file, buffer switch) are drawn.
;;; "center"     — a floating box in the middle of the window, telescope-style
;;; "bottom"     — a list growing up from the bottom edge, consult-style
;;; "minibuffer" — one plain line at the bottom, the vim prompt
(set-completion-style "center")

;;; ---------------------------------------------------------------------------
;;; Modeline
;;;
;;; What the strip says is a list of little templates, and the shipped list is
;;; `default-modeline' in `library.lisp' — which has already run by the time you
;;; read this. To change it, clear it and say your own:
;;;
;;;   (clear-modeline)
;;;   (modeline-segment :left  " %m " :face :mode :bold t :filled t)
;;;   (modeline-segment :left  "  %b" :bold t)
;;;   (modeline-segment :right "%l:%c" :bold t)
;;;
;;; %m is the modal state, %b the buffer, %+ the unsaved dot, %M the major mode,
;;; %l and %c the position, %p Top/Bot/All/a percentage. `modeline-segment' in
;;; `library.lisp' documents all of them. A segment whose codes all come back
;;; empty disappears with its own separators, so nothing needs a condition around
;;; it — "  %P" carries its two spaces and leaves with them on a buffer that has
;;; no file behind it.
;;;
;;; To keep the shipped strip and only add to it, skip the `clear-modeline'.

;;; ---------------------------------------------------------------------------
;;; Dashboard
;;;
;;; The banner is plain text and the renderer centres it; the logo goes above it.
;;; The *items* are further down, after the keymap, because each row prints the
;;; leader sequence bound to its command and the table is not complete until the
;;; bindings have been made.

(dashboard-banner (%banner))
(dashboard-default-logo 10)

;;; ---------------------------------------------------------------------------
;;; Keys
;;;
;;; Modes: "normal" "insert" "visual" "visual-line" "visual-block" "magit"
;;; "dashboard". Sequences are space-separated tokens: SPC, C-x, <esc>, <ret>,
;;; <tab>, or a literal key. These are consulted before the built-in vim
;;; grammar, so config wins.
;;;
;;; `define-leader' binds a SPC-prefixed sequence in the modes that have a
;;; leader; `define-key-everywhere' binds a chord in all of them.

;;; Leader bindings work with a selection up, not just from normal mode.
(define-leader "SPC f f" "find-file")
(define-leader "SPC f s" "save-file")
(define-leader "SPC b d" "show-dashboard")
(define-leader "SPC b b" "switch-buffer")
(define-leader "SPC j j" "switch-buffer")
(define-leader "SPC t t" "theme")             ; `SPC t' is the toggle group
(define-leader "SPC h r" "reload-config")
(define-leader "SPC h v" "lisp-version")
(define-leader "SPC h m" "messages-buffer")   ; what Emacs puts on `C-h e'
(define-leader "SPC b s" "lisp-scratch")
(define-leader "SPC q q" "quit")
(define-key-everywhere "C-M-j" "switch-buffer")

;;; `M-o' jumps between windows, ace-window style: with two it just switches,
;;; with more it labels each pane and waits for you to press a label.
;;; `C-s' is consult-line — pick a line by fuzzy match, with the buffer
;;; previewing as you narrow, and Esc putting the cursor back.
(define-key-everywhere "M-o" "ace-window")
(define-key-everywhere "C-s" "search-line")
;;; `C-g' is consult-ripgrep, not quit — Esc is what aborts here. Candidates
;;; come from `rg' itself, so the pattern is a real regex and the fuzzy filter
;;; stays out of the way rather than second-guessing it.
(define-key-everywhere "C-g" "search-project")
(define-leader "SPC s l" "search-line")
(define-leader "SPC s p" "search-project")

;;; avy: `SPC j c' types a character and labels every one of them on screen, so
;;; two keystrokes land you anywhere the eye can already see. It goes in the
;;; `SPC j' jump group beside `SPC j j', and *not* on `/', which is what avy's
;;; own README suggests and what doom binds: `/' is search here, in the editor
;;; and in the muscle memory of everyone who arrives from vim, and taking it
;;; would mean rehoming search to buy a key avy does not need. Move it if you
;;; disagree — the command is the same either way, and search would want `C-s',
;;; which is already `search-line'.
(define-leader "SPC j c" "avy-goto-char")

;;; Projects. The root is found from the *current buffer* — the file on screen
;;; is the only honest answer to "which project" when two are open at once —
;;; by walking up for a `.git', `Cargo.toml', `package.json' and the like. A
;;; VCS root beats a build file, so a workspace member resolves to the repo.
;;;
;;; `SPC p p' switches: the candidates are projects visited before, and picking
;;; one opens it as a directory, which is dired. Finding a file and switching
;;; project are the same prompt because opening a root *is* switching to it.
(define-leader "SPC p f" "project-find-file")
(define-leader "SPC p p" "project-switch")
;;; `SPC p o' is the way out of the remembered list: it prompts for a path,
;;; starting at `~/', and completes a directory at a time as you type — so a
;;; project you have never opened is reachable without having opened it. What
;;; you pick opens in dired and joins the `SPC p p' list. `SPC p D' is the same
;;; gesture inside the current project.
(define-leader "SPC p o" "project-open")
(define-leader "SPC p D" "project-find-dir")
(define-leader "SPC p d" "project-dired")
(define-leader "SPC p c" "project-compile")   ; cargo build, npm run build, make
(define-leader "SPC p t" "project-test")
(define-leader "SPC p r" "project-root")      ; echo it, with what identified it
;;; `SPC p g' greps into a *buffer* — every hit at once, `RET' to open one and
;;; `r' to replace across the lot. `SPC /' is the same ripgrep through a picker,
;;; for when you want one hit and not the list. `project-forget' moved to
;;; `SPC p F': re-walking the file cache is a thing you do once a month and this
;;; is a thing you do all day.
(define-leader "SPC p g" "project-grep")
(define-leader "SPC p F" "project-forget")    ; re-walk after creating files
(define-key-everywhere "C-M-p" "project-find-file")
(define-leader "SPC w w" "ace-window")

;;; The third door into a project — a git URL — landing you where the other two
;;; do. `*project-directory*' is where the checkout goes; set it above this line
;;; to keep your clones somewhere other than `~/Code'.
(define-leader "SPC p n" "project-clone")

;;; `SPC p c' runs *the* build; this reads the Makefile and asks which target.
;;; The output pane's whole keymap is `q', which is what every read-only buffer
;;; in Emacs is dismissed with — the window goes, the buffer and the child stay,
;;; so a build you dismissed early is still in the switcher when you want to
;;; know how it ended.
;;;
;;; It opens *below* rather than beside: compiler output is lines, a rustc error
;;; is a path and a caret and a note wrapped to whatever width it is given, and
;;; half a frame is not enough of one.
(define-leader "SPC p m" "project-make")
(define-key "terminal-output-mode" "q" "delete-window")

;;; The terminal. A real shell on a real PTY, in a buffer.
;;;
;;; In `terminal' mode the shell owns the keyboard: `d', `j', Esc and above all
;;; `C-c' all reach the child, because a `C-c' that stopped at the editor would
;;; mean never being able to interrupt anything. That is why this is the one
;;; mode whose keymap is consulted *instead of* the Evil grammar rather than
;;; before it — and why only bindings made here, in "terminal", are live.
;;;
;;; `C-M-t' is the way out, into Normal mode on the same buffer, where the
;;; motions work and the scrollback can be read. The mouse wheel scrolls the
;;; history either way.
(define-leader "SPC o t" "terminal")
(define-key "terminal" "C-M-t" "terminal-normal")
;;; ...and back in, the way `i' enters Insert mode from Normal.
(define-key "normal" "C-M-t" "terminal")

;;; Copy and paste across the boundary.
;;;
;;; Only paste needs a key. Copying is already whole: `C-M-t' freezes the
;;; scrollback into an ordinary buffer, where `v'/`V' select and `y' yanks, and
;;; the unnamed register is mirrored to the system clipboard every frame — so
;;; what you yank out of a shell is on the pasteboard before you let go of `y'.
;;; The gutter stays off the whole time: a terminal buffer is a grid the child
;;; owns and has no line numbers to draw, whatever `set-line-numbers' says.
;;;
;;; `M-v' is ⌘V — a Command chord, which is the only kind that reaches the
;;; editor from inside a session, and which no shell has ever been able to see.
;;; Bracketing is decided by the child: `terminal-paste' marks the text as a
;;; paste when the program asked for that mode, so a multi-line yank lands in
;;; the line editor instead of running every line but the last.
;;;
;;; **It is `terminal-paste-image' on the key and not `terminal-paste'**, and
;;; the two are one gesture rather than two: a screenshot in the clipboard is
;;; written out and its *path* typed into the child, and anything else falls
;;; through to the ordinary text paste. That is what a coding agent takes a
;;; picture as — every harness reads one from a path in its prompt — so ⌘V into
;;; an agent means "here is what I am looking at" without a second key to
;;; remember or a decision to make about which one this is.
;;;
;;; Dragging a file from Finder onto a terminal pane does the same thing, and
;;; needs no binding: the drop is an event, and over a session it types the path
;;; instead of opening the file in a pane, which is not what anyone means by
;;; dragging a PNG onto a chat.
(define-key "terminal" "M-v" "terminal-paste-image")
(pushnew "terminal-paste" *extra-commands* :test #'string=)
(pushnew "terminal-paste-image" *extra-commands* :test #'string=)

;;; Dired. `SPC f d' opens the directory of the current file; in a listing,
;;; the keys are Emacs' own.
(define-leader "SPC f d" "dired")
(define-key "dired" "<ret>" "dired-enter")
(define-key "dired" "-" "dired-up")
(define-key "dired" "^" "dired-up")
(define-key "dired" "m" "dired-mark")
(define-key "dired" "u" "dired-unmark")
(define-key "dired" "t" "dired-toggle-marks")
(define-key "dired" "d" "dired-flag-delete")
(define-key "dired" "x" "dired-execute")
;;; `D' deletes now, where `d'+`x' flags and expunges — Emacs has both for the
;;; reason both are worth having: `x' is the batch you built up on purpose, and
;;; `D' is the one file you are looking at. `D' takes the `*' marks if there are
;;; any, so it is also "delete what I selected" without a second flagging pass.
;;;
;;; Both ask first, and both ask by naming what goes — a count for several, the
;;; filename for one. That is the whole value: `D' on the wrong line is a
;;; keystroke away from `d', and neither of them is undoable.
(define-key "dired" "D" "dired-delete")
;;; `u' clears the mark under the cursor and `t' inverts every mark; neither is
;;; "I have lost track of what is marked", which is what `U' is for.
(define-key "dired" "U" "dired-unmark-all")
;;; Emacs' `w': the file's *name* into the register, which is this editor's kill
;;; ring and its clipboard both — so `p' pastes it and so does ⌘V elsewhere.
(define-key "dired" "w" "dired-copy-filename")
(define-key "dired" "R" "dired-rename")
(define-key "dired" "C" "dired-copy")
(define-key "dired" "+" "dired-mkdir")
;;; `C-c n' makes an empty file. Emacs has no single key for this at all — `+'
;;; is the directory — and a chord works here only because `C-c' is a global
;;; *prefix* rather than a whole binding; see the `C-c C-c' note further down.
(define-key "dired" "C-c n" "dired-create-file")
(define-key "dired" "H" "dired-toggle-hidden")
;;; `g r', not a bare `g': a single-key binding here would claim the `g' that
;;; starts `gg', and the second one would only refresh again — so the motion
;;; every other buffer has would be the one thing a listing could not do. `g r'
;;; is what evil-collection binds refresh to for the same reason, and `g' stays
;;; a prefix, so `gg' falls through to the grammar underneath.
(define-key "dired" "g r" "dired-refresh")
(define-key "dired" "q" "show-dashboard")

;;; ---------------------------------------------------------------------------
;;; Magit
;;;
;;; `magit-*' are built-in verbs, run by the editor rather than by this image.
;;; The status buffer has its own mode, which is what lets `s', `u' and `c' mean
;;; stage, unstage and commit there while still meaning substitute, undo and
;;; change everywhere else — a binding is consulted before the built-in grammar,
;;; so the motions (j k gg G /) keep working in the status buffer too.
(define-leader "SPC g g" "magit-status")
(define-leader "SPC g s" "magit-status")

(define-key "magit" "s" "magit-stage")
(define-key "magit" "u" "magit-unstage")
(define-key "magit" "S" "magit-stage-all")
(define-key "magit" "U" "magit-unstage-all")
(define-key "magit" "g r" "magit-refresh")   ; `g' stays a prefix, so `gg' works
(define-key "magit" "q" "show-dashboard")
;;; `TAB' is the one that makes it a buffer rather than a list: on a section it
;;; folds, on a file it opens the diff, on a commit it opens that commit. With a
;;; diff open, `s' and `u' act on the *hunk* under the cursor — staging part of
;;; a file is what magit is used for more than anything else.
(define-key "magit" "<tab>" "magit-toggle")
;;; A rebase in flight. Stopping on a conflict is ordinary progress, not an
;;; error: fix the files, stage them, then `r c'.
(define-key "magit" "r c" "magit-rebase-continue")
(define-key "magit" "r s" "magit-rebase-skip")
(define-key "magit" "r a" "magit-rebase-abort")   ; throws the rebase away

;;; The rest of the keymap — commit, branch, stash, push, merge, reset, the
;;; conflict keys and the four commands that ask for a name — is in
;;; `modes/magit.lisp', which ships with the editor rather than with this file.
;;; It is one keymap either way: a binding is a name in a table, and both files
;;; write to the same one.
;;;
;;; `c', `P' and `F' are *prefixes* there and are deliberately not bound whole
;;; here. Core resolves an exact binding before it asks whether a sequence is a
;;; prefix, so a bare `c' would make `c a' unreachable — which is exactly what
;;; had quietly happened to `c a' before magit grew the rest of its `c' family.

;;; C-c stays one binding — `eval-dwim' — and finishes the commit when the
;;; buffer is a commit message. Binding C-c to `magit-commit-finish' outright
;;; would take it away from every other buffer, and giving the message buffer
;;; its own mode would lose the binding the moment you pressed `i' to type.

;;; C-c evaluates Lisp, from anywhere. `eval-dwim' is a built-in verb resolved
;;; by the editor, not a function in the image: it evaluates the live buffer —
;;; the selection if there is one, else the top-level form under point, else the
;;; whole buffer — so nothing needs saving first.
;;;
;;; `C-c C-c' and not a bare `C-c', so that `C-c' is a *prefix* — which is what
;;; it is in Emacs, and what the whole `C-c <letter>' family below depends on.
;;;
;;; This is a rule about core, not a preference. `normal_key' looks for an exact
;;; global binding *before* it asks whether the sequence is a prefix, so while
;;; `C-c' was bound whole no global `C-c d' could ever be typed: the first key
;;; fired and the second landed in a fresh sequence. Only a *mode-local* prefix
;;; outranked an exact global one, which is why `C-c C-e' worked in a Lisp
;;; buffer and nowhere else.
;;;
;;; Nothing is lost by the move. `C-c C-c' is Emacs' own spelling for "do the
;;; thing this buffer is for", it is already what `lisp-mode' binds to
;;; `lisp-eval-defun', and it is what finishes a commit message — `eval-dwim'
;;; dispatches on the buffer kind, so one binding still covers both.
(define-key-everywhere "C-c C-c" "eval-dwim")

;;; ...except while typing, where it stays a single key. `insert_key' consults
;;; only *single-key* bindings and only in the `insert' keymap — it never waits
;;; for a second — so `C-c C-c' is unreachable there and a prefix would silently
;;; do nothing. Keeping the one-key binding in Insert also keeps the property
;;; the paragraph this replaced was describing: `C-c' evaluates rather than
;;; leaving Insert mode, and `<esc>' and `C-g' are still how you leave.
(define-key "insert" "C-c" "eval-dwim")

;;; The `C-c' family. These are the bindings a hand reaches for without
;;; deciding to, so they get the shortest thing that is not already spoken for.
;;;
;;; Every one of these already had a leader spelling — `SPC p p', `SPC a a' —
;;; and keeps it. A leader sequence is discoverable: hold `SPC' and which-key
;;; shows you the family. A chord is *fast*, and the two are worth having for
;;; different reasons, so this adds rather than replaces.
(define-key-everywhere "C-c d" "dired")          ; this file's directory
(define-key-everywhere "C-c t" "terminal")
(define-key-everywhere "C-c m" "magit-status")
(define-key-everywhere "C-c p" "project-switch")
(define-key-everywhere "C-c c" "project-find-file")
(define-key-everywhere "C-c a" "ai")
(define-key-everywhere "C-c i" "edit-config")
(define-key-everywhere "C-c s" "switch-buffer")
(define-key-everywhere "C-c b" "messages-buffer")
(define-key-everywhere "C-c y" "yank-buffer-file-name")
(define-key-everywhere "C-q" "delete-window")

;;; The `C-x' family, spelled as Emacs spells it.
;;;
;;; A prefix rather than more `C-c' chords, because these three are the ones
;;; whose Emacs spelling is muscle memory — `C-x C-s' in particular is typed by
;;; people who have never read a keymap in their lives. `C-c' stays the
;;; editor's own family; this is the one borrowed wholesale.
;;;
;;; Bound everywhere, Insert included: saving is not something to leave a mode
;;; for. `C-x C-s' reaches `save-file' with no argument, which means "in place"
;;; — see the wrapper in `shim.c'.
(define-key-everywhere "C-x C-s" "save-file")
(define-key-everywhere "C-x k" "kill-buffer")
(define-key-everywhere "C-x w" "delete-window")

;;; `execute-command' and `switch-buffer' are built-in verbs — core opens the
;;; prompt itself, so these names are not Lisp functions and are not in the M-x
;;; list. `SPC ;' is the usual leader spelling for M-x.
;;; "dashboard" is in this list on purpose: it is the mode the editor *opens*
;;; in, so leaving it out means M-x does nothing until you have already entered
;;; a buffer — which reads as M-x being broken.
(define-key-everywhere "M-x" "execute-command")
(define-leader "SPC ;" "execute-command")
(define-key "dashboard" "f" "find-file")
(define-key "dashboard" "b" "switch-buffer")

;;; Magnify *this window*. Meta is Command (⌘) first, with Option as a fallback,
;;; so `M-+' is ⌘-Shift-= ; `M-=' is the same key without the Shift, and works
;;; on any keyboard layout. Bound in Insert mode too, so zooming does not
;;; require leaving what you were typing.
;;;
;;; The window and not the frame, because the reason to magnify is a *document*:
;;; reading a paper beside the code you are writing about it wants one pane
;;; larger and the other exactly as it was. Four steps — 100, 125, 150, 200 per
;;; cent — and they are a fixed set rather than a free size for a reason worth
;;; knowing: the renderer's face cache is keyed by point size and its bound *is*
;;; the number of sizes that can exist, so these are the same four steps an
;;; overlay's `scale' may already ask for.
;;;
;;; `set-font-size' is untouched and is still the size of text *everywhere* — a
;;; window's zoom multiplies it. `M-x text-scale-increase' is that global knob;
;;; these keys are the local one, which is the one you reach for far more often.
(define-key-everywhere "M-+" "zoom-in")
(define-key-everywhere "M-=" "zoom-in")
(define-key-everywhere "M--" "zoom-out")
(define-key-everywhere "M-0" "zoom-reset")

;;; Org markup, only in org buffers and only with something selected.
(define-key "org-mode" "SPC m b" "org-bold")
(define-key "org-mode" "SPC m i" "org-italic")
(define-key "org-mode" "SPC m c" "org-code")

;;; LaTeX previews. `C-c r' is what Emacs muscle memory wants, and it works
;;; because `normal_key' lets a mode-local *prefix* outrank a global exact
;;; binding — `C-c' still evaluates everywhere else, including in org buffers on
;;; its own. `SPC m l' is the leader spelling, and `M-x org-latex-preview' works
;;; from anywhere: all of these are ordinary zero-argument functions.
;;;
;;; The commands are in `modes/org-latex.lisp'; a binding names a string and is
;;; resolved when the key is pressed, so these may be made before it loads.
(define-key "org-mode" "C-c r" "org-latex-preview")
(define-key "org-mode" "C-c R" "org-latex-preview-clear")
(define-key "org-mode" "SPC m l" "org-latex-preview")
(define-key "org-mode" "SPC m L" "org-latex-preview-clear")

;;; The agenda — every unfinished item across `*org-agenda-files*', or across
;;; the buffer you are in when that is unset. It answers into the `*xref*'
;;; listing, so `RET' on a row opens the headline and `q' puts the list away.
;;;
;;; `g' for aGenda and not `a', which `org-modern-appear' has: every letter that
;;; says "agenda" is taken in org buffers, and a binding that quietly replaced
;;; another mode's is worse than one you have to learn.
(define-key "org-mode" "SPC m g" "org-todo-list")
(define-key "org-mode" "SPC m G" "org-agenda-tags")

;;; Tables need no key of their own. `TAB' aligns on its way between cells, and
;;; `C-c C-c' on a table aligns it where it stands — org's own gesture, added to
;;; the front of `org-ctrl-c-ctrl-c'.

;;; ---------------------------------------------------------------------------
;;; The dashboard menu
;;;
;;; Down here rather than up beside the banner because of the third column:
;;; every row prints the leader sequence that runs the same command, so the
;;; startup screen teaches the keymap instead of being a menu you use once and
;;; then never see the point of again. `dashboard-item' reads the table
;;; `define-leader' built, and the table is only complete now.

(clear-dashboard-items)
;; Built-in verbs...
(dashboard-item #\f "Find file"      "find-file")
;; ...and functions from the library, on equal footing. `lisp-scratch' rather
;; than the built-in `scratch' verb, which only drops you in an empty,
;; language-less buffer nothing can evaluate.
(dashboard-item #\s "Scratch buffer" "lisp-scratch")
(dashboard-item #\e "Evaluate Lisp"  "eval-dwim")
(dashboard-item #\p "Open project"   "project-switch")
(dashboard-item #\t "Change theme"   "theme")
(dashboard-item #\c "Edit configuration" "edit-config")
(dashboard-item #\r "Reload configuration" "reload-config")
(dashboard-item #\q "Quit" "quit")

;;; ---------------------------------------------------------------------------
;;; The rest of the runtime
;;;
;;; which-key, Common Lisp editing, a REPL, org's markup drawn rather than
;;; typed, the maths curriculum, the AI harnesses and the tutorial — the whole
;;; shipped set, in the order `*runtime-modules*' declares. That list is in
;;; `library.lisp' rather than here on purpose: it is what stops a config you
;;; wrote a year ago from missing everything added since.
;;;
;;; `(setf *runtime-modules* (remove "modes/tutor.lisp" *runtime-modules*))'
;;; above this line drops one; `(push "my-mode.lisp" (cdr (last ...)))' — or
;;; simply a `load' of your own after it — adds one.
;;;
;;; Loaded here, after the settings and the keymap, because a mode may read
;;; either: `lsp.lisp' installs `after-change-hook' and there is no reason for
;;; that to fire while the config is still being read.

(load-runtime-modules)

;;; `g d' is the vim spelling and wins over the built-in grammar, which is what
;;; a binding in this file always does. The `SPC l' family is the leader
;;; spelling for the rest. Guarded because a config may have dropped `lsp.lisp'
;;; from `*runtime-modules*', or the load may have failed and said so.
(when (fboundp 'lsp-goto-definition)
  ;; The `g' family is the vim spelling and the one your hands already know:
  ;; `g d' definition, `g r' references, `g D' declaration, `g i'
  ;; implementation, `g y' the type. `K' is documentation, which is what `K' has
  ;; meant in vim since before any of this existed — it ran `man'.
  (define-key "normal" "g d" "lsp-goto-definition")
  (define-key "normal" "g r" "lsp-find-references")
  (define-key "normal" "g D" "lsp-goto-declaration")
  (define-key "normal" "g i" "lsp-goto-implementation")
  (define-key "normal" "g y" "lsp-goto-type-definition")
  (define-key "normal" "K" "lsp-hover")
  (define-leader "SPC l l" "lsp")
  (define-leader "SPC l d" "lsp-goto-definition")
  (define-leader "SPC l R" "lsp-find-references")
  (define-leader "SPC l k" "lsp-hover")
  (define-leader "SPC l n" "lsp-rename")
  (define-leader "SPC l o" "lsp-document-symbols")   ; a symbol in this file
  (define-leader "SPC l w" "lsp-workspace-symbols")  ; one anywhere in the project
  (define-leader "SPC l e" "lsp-diagnostics-at-point")
  (define-leader "SPC l E" "lsp-list-diagnostics")
  (define-leader "SPC l r" "lsp-restart")
  (define-leader "SPC l q" "lsp-stop")
  (define-leader "SPC l s" "lsp-status"))

;;; corfu, in Insert mode, spelled vim's way rather than Emacs': `C-n' and `C-p'
;;; cycle, `C-y' takes the one that is lit, `C-e' gives up.
;;;
;;; Four keys that mean *nothing at all* in Insert mode today — core answers a
;;; bare Ctrl with no command — so none of these takes anything away.
;;;
;;; `TAB', `S-TAB' and `RET' do the same three jobs and are deliberately absent
;;; from this list: they are not bindable from the image at all, because a Lisp
;;; fallback for "no popup was up" lands a queue turn late and would put the
;;; newline *after* the character you typed next. Core asks the question itself
;;; and calls the same three commands — see the note in `lsp.lisp'.
;;;
;;; `C-M-i' is Emacs' `completion-at-point' and asks explicitly, which is how you
;;; get the list one character into a word.
(when (fboundp 'lsp-complete)
  (define-leader "SPC l c" "lsp-complete")
  (define-key "insert" "C-M-i" "lsp-complete")
  (define-key "insert" "C-n" "lsp-complete-next")
  (define-key "insert" "C-p" "lsp-complete-previous")
  (define-key "insert" "C-y" "lsp-complete-accept")
  (define-key "insert" "C-e" "lsp-complete-abort"))

;;; A third language server is one line, with no Rust to rebuild — `pylsp' and
;;; `clangd' ship, and the mode registry is what the first argument names:
;;;
;;;   (lsp-register-server 'rust-mode "rust-analyzer")
;;;   (lsp-register-server 'go-mode "gopls")
;;;
;;; A server that is not installed reports in the status line the first time a
;;; buffer in its mode is touched, and nothing else breaks.

;;; ---------------------------------------------------------------------------

;;; Last, so that every function defined above is in the list.
(refresh-commands)

(message (format nil "zemacs: init.lisp loaded — ~a ~a is driving the editor."
                 (lisp-implementation-type)
                 (lisp-implementation-version)))
