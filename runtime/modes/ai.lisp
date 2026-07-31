;;;; ai.lisp — coding agents as buffers.
;;;;
;;;; A harness — Claude Code, Cursor, opencode — is a full-screen program on a
;;;; PTY, which is a thing zemacs already has: `crates/term' runs one, and since
;;;; sessions became plural it runs as many as you like, each in its own buffer.
;;;; So there is no "AI backend" in Rust and there is not going to be one. What
;;;; is here is the part that was actually missing: *which* programs to offer,
;;;; *how* each one spells "resume", and a menu to pick between them.
;;;;
;;;; That is the standing rule — Rust provides fast primitives, features get
;;;; written in Lisp — and this is as clean a case as it gets. A fourth harness
;;;; is one line at the bottom of `*ai-harnesses*'; it is not a match arm, a
;;;; recompile, or anything anybody has to read Rust to add.
;;;;
;;;; The whole editor-side contract is one verb string:
;;;;
;;;;   (call-command "terminal-run:NAME:PROGRAM ARG ARG")
;;;;
;;;; which forks PROGRAM on a PTY, gives it a buffer called `*NAME*', and hands
;;;; it the keyboard. Everything below builds that string.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; The harnesses
;;;
;;; `(NAME PROGRAM NEW-ARGS RESUME-ARGS)':
;;;
;;;   NAME         what the buffer is called — `claude' gives `*claude*', and a
;;;                second session of the same harness becomes `*claude*<2>'.
;;;   PROGRAM      the binary, looked up on $PATH. Not installed is *said*, in
;;;                the menu, rather than discovered as a buffer that appears
;;;                and vanishes.
;;;   NEW-ARGS     how to start a fresh conversation. Usually nothing.
;;;   RESUME-ARGS  how to pick up an old one.
;;;
;;; The resume flags below were read out of each tool's own `--help' rather
;;; than remembered, because all three have changed them at least once:
;;;
;;;   claude -r            "Resume a conversation by session ID, or open
;;;                        interactive picker with optional search term". With
;;;                        no id it puts *its own* picker up — which is the
;;;                        right answer, since it knows which sessions exist
;;;                        and we do not. (`-c'/`--continue' is the other one:
;;;                        the most recent conversation in this directory, no
;;;                        picker.)
;;;   cursor-agent --resume
;;;                        "Select a session to resume" — again a picker with
;;;                        no id, and `--continue' for the previous session
;;;                        without one. The binary is `cursor-agent', not
;;;                        `cursor', which is the GUI.
;;;   opencode --continue  "continue the last session". opencode is the one
;;;                        with no interactive resume *flag*: sessions are
;;;                        listed by `opencode session list' and picked inside
;;;                        the TUI, so `-c' is the closest thing it has and
;;;                        `-s ID' is the exact form.
;;;
;;; ponytail: arguments are split on whitespace on the way through the verb, so
;;; a flag whose value contains a space cannot be expressed here. Nothing any
;;; of the three needs for *resuming* has one. The upgrade is the verb carrying
;;; a list instead of a string, and it is worth doing the first time a harness
;;; wants `--prompt "do the thing"'.

(defvar *ai-harnesses*
  (list (list "claude"   "claude"       '()   '("-r"))
        (list "cursor"   "cursor-agent" '()   '("--resume"))
        (list "opencode" "opencode"     '()   '("--continue")))
  "The coding agents `ai' offers, as (NAME PROGRAM NEW-ARGS RESUME-ARGS).
Add one with `ai-add-harness'.")

(defun ai-add-harness (name program &optional new-args resume-args)
  "Offer NAME in the `ai' menu, running PROGRAM.
Replaces an existing entry with the same NAME, so re-loading the config does
not double the list.

  (ai-add-harness \"aider\" \"aider\" nil '(\"--restore-chat-history\"))"
  (setf *ai-harnesses*
        (append (remove name *ai-harnesses* :key #'first :test #'string=)
                (list (list name program new-args resume-args))))
  name)

;;; ---------------------------------------------------------------------------
;;; Is it installed?
;;;
;;; `which', in Lisp, because the only thing it needs is $PATH and `probe-file'
;;; and both are already here. The editor does the same check in Rust before it
;;; forks — this one exists so the *menu* can say so, which is the difference
;;; between picking a harness and finding out, and picking it and watching a
;;; buffer flash.

(defun %ai-split (string char)
  "STRING split on CHAR. Empty fields are kept; the caller drops them."
  (loop with start = 0
        for i = (position char string :start start)
        collect (subseq string start i)
        while i do (setf start (1+ i))))

(defun executable-find (program)
  "Where PROGRAM is on $PATH, as a pathname, or NIL.
A name containing a separator is a path and is taken as one, which is what
every shell does."
  (if (find #\/ program)
      (probe-file program)
      (dolist (dir (%ai-split (or (ext:getenv "PATH") "") #\:))
        (when (plusp (length dir))
          (let ((path (probe-file (concatenate 'string dir "/" program))))
            ;; `probe-file' answers for a *directory* of that name too, and its
            ;; truename has a NIL name component — which is how a directory
            ;; called `claude' on $PATH would otherwise read as an installed
            ;; harness.
            (when (and path (pathname-name path))
              (return path)))))))

(defun ai-installed-p (name)
  "Whether the harness called NAME has its binary on $PATH."
  (let ((h (assoc name *ai-harnesses* :test #'string=)))
    (and h (executable-find (second h)) t)))

;;; ---------------------------------------------------------------------------
;;; The menu
;;;
;;; Two questions, because there are two decisions and mashing them into one
;;; nine-item list is how a menu stops being readable. `completing-read' answers
;;; on a later turn of the Lisp queue — see threading.org — so the second
;;; question is asked from inside the first one's callback, which the prompt
;;; machinery explicitly supports.

(defun %ai-label (harness)
  (destructuring-bind (name program new resume) harness
    (declare (ignore new resume))
    (if (executable-find program)
        name
        ;; Offered anyway, and refused with a reason. A harness silently
        ;; missing from the menu is indistinguishable from one you spelled
        ;; wrong in your config.
        (format nil "~a  (not installed: ~a)" name program))))

(defun %ai-verb (harness resume)
  "The editor verb that starts HARNESS. RESUME picks its argument list.
Separate from `%ai-start' so the string can be read without forking anything —
which is what makes it testable and what makes it obvious that this file's
entire contribution is one line of text."
  (destructuring-bind (name program new-args resume-args) harness
    (format nil "terminal-run:~a:~a~{ ~a~}"
            name program (if resume resume-args new-args))))

(defun %ai-start (harness resume)
  "Fork HARNESS. RESUME picks between its two argument lists."
  (if (null (executable-find (second harness)))
      ;; The editor refuses this too, before it forks. Saying it here as well
      ;; means the answer comes from the thing you picked rather than from a
      ;; buffer that appeared and vanished.
      (message (format nil "~a is not installed — no ~a on $PATH"
                       (first harness) (second harness)))
      (call-command (%ai-verb harness resume))))

(defun ai ()
  "Pick a coding agent, then start a new conversation or resume one.
Bound to `C-a'."
  (completing-read "Agent: " (mapcar #'%ai-label *ai-harnesses*)
    (lambda (pick)
      (when pick
        ;; The label carries the "(not installed)" note, so the harness is
        ;; found by prefix rather than by equality.
        (let ((harness (find-if (lambda (h)
                                  (let ((name (first h)))
                                    (and (<= (length name) (length pick))
                                         (string= name pick
                                                  :end2 (length name)))))
                                *ai-harnesses*)))
          (if (null harness)
              (message (format nil "no such agent: ~a" pick))
              (completing-read (format nil "~a: " (first harness))
                               '("new" "resume")
                (lambda (what)
                  (when what
                    (%ai-start harness (string= what "resume")))))))))))

;;; A shorthand per harness, so a key can go straight to one without the menu.
;;; Generated rather than written out, because the list is the list.
(dolist (harness *ai-harnesses*)
  (let ((name (first harness)))
    (setf (symbol-function (intern (string-upcase (format nil "ai-~a" name)) :zemacs))
          (let ((name name))
            (lambda ()
              (let ((h (assoc name *ai-harnesses* :test #'string=)))
                (when h (%ai-start h nil))))))))

;;; ---------------------------------------------------------------------------
;;; AI mode
;;;
;;; `(major-mode)' answers "ai-mode" in a harness session and "terminal-mode" in
;;; a plain shell, and the editor fires `ai-mode-hook' once when a session
;;; starts. That is the whole mode: everything else it needs is session
;;; management, which is the same in a shell and is therefore bound for both.
;;;
;;; The bindings are the delicate part and the reason to read `model.org' first.
;;; "terminal" is the one modal state consulted *instead of* the vim grammar,
;;; because `d', `j' and above all `C-c' have to reach the child — and an agent
;;; needs that even more than a shell does, since you are typing English at it.
;;; So nothing here takes a bare key inside a session. Everything is a Command
;;; chord, which `Key::is_editor_key' already reserves for the editor and which
;;; no terminal program has ever been able to see.
;;;
;;; ponytail: these are bound in the "terminal" *state*, not in `ai-mode', so a
;;; shell gets them too. `terminal_key' in core consults only the state's keymap
;;; — not the buffer's major-mode map, the way `normal_key' does — so a
;;; mode-local binding in a terminal would silently never fire. Every verb here
;;; is meaningful in a shell as well, so the honest fix (layer the major-mode
;;; map into `terminal_key') buys nothing yet and is one `if' in `crates/core'
;;; when it does.

(defun ai-mode-hook ()
  "Runs once when a session becomes an agent rather than a shell."
  (message (format nil "~a — C-M-t to step out, C-M-r to restart"
                   (buffer-name))))

;;; Jump to a file the agent mentioned.
;;;
;;; Agents print paths the way every tool does — `crates/app/src/term.rs:42' —
;;; and `find-file-at' already knows how to open `path:line:text' relative to
;;; the project root, because that is what a ripgrep hit looks like. So this is
;;; a *parser*, not a feature: pull the token out from under the cursor, strip
;;; the punctuation a sentence wrapped around it, and hand it over.
;;;
;;; Works in the frozen view — `C-M-t' first — because that is where the cursor
;;; is yours. Inside a live session the cursor belongs to the child.

(defun %ai-token-at (line col)
  "The run of non-blank characters around COL in LINE."
  (let* ((len (length line))
         (col (max 0 (min col (max 0 (1- len)))))
         (blank (lambda (c) (member c '(#\Space #\Tab) :test #'char=))))
    (if (or (zerop len) (funcall blank (char line col)))
        ""
        (let ((start col)
              (end col))
          (loop while (and (plusp start) (not (funcall blank (char line (1- start)))))
                do (decf start))
          (loop while (and (< (1+ end) len) (not (funcall blank (char line (1+ end)))))
                do (incf end))
          (subseq line start (1+ end))))))

(defun %ai-clean-path (token)
  "TOKEN with the punctuation prose wraps around a path taken off both ends."
  (let ((junk '(#\( #\) #\[ #\] #\{ #\} #\" #\' #\` #\, #\. #\; #\: #\! #\?
                #\* #\< #\>)))
    (string-trim junk token)))

(defun ai-find-file-at-point ()
  "Open the path under the cursor, at its line number if it has one.
For reading what an agent just said: `src/main.rs:120' opens that file on that
line, and a bare path opens at the top."
  (let* ((token (%ai-clean-path (%ai-token-at (line-string) (column))))
         ;; `path:line' is the shape every tool prints. Split from the right so
         ;; an absolute path's own colons — Windows drives, URLs — do not eat
         ;; the filename, and fall back to the whole token when the tail is not
         ;; a number.
         (colon (position #\: token :from-end t))
         (tail (and colon (subseq token (1+ colon))))
         (number (and tail (plusp (length tail))
                      (every #'digit-char-p tail)
                      tail)))
    (cond ((zerop (length token)) (message "no path under the cursor"))
          ;; `find-file-at' wants `path:line:text' and gives up on anything with
          ;; fewer than two fields, so a bare path is handed a line of 1.
          (number (find-file-at (format nil "~a:~a:" (subseq token 0 colon) number)))
          (t (find-file-at (format nil "~a:1:" token))))))

;;; ---------------------------------------------------------------------------
;;; Keys
;;;
;;; `C-a' is the menu, everywhere the vim grammar is live. Deliberately *not* in
;;; "terminal" or "insert": `C-a' is beginning-of-line to readline and to every
;;; agent's own input box, and taking it inside a session would be exactly the
;;; mistake `C-c' is documented as avoiding. Inside a session the menu is
;;; `C-M-a', which no child can see.

(dolist (mode '("normal" "visual" "visual-line" "visual-block"
                "dashboard" "magit" "dired"))
  (define-key mode "C-a" "ai"))

;;; `C-M-' chords reach the editor from *inside* a terminal — they fall through
;;; to the Normal keymap via `Key::is_editor_key' — so binding them once in
;;; "normal" makes them live everywhere, session or not.
(define-key "normal" "C-M-a" "ai")

;;; Session management. Bound in "terminal" so they are unambiguous there, and
;;; in "normal" so they also work from the frozen view and from a file buffer.
(dolist (mode '("terminal" "normal"))
  (define-key mode "C-M-n" "terminal-next")
  (define-key mode "C-M-b" "terminal-prev")
  (define-key mode "C-M-r" "terminal-restart")
  (define-key mode "C-M-w" "terminal-close"))

;;; ...and the leader, for the ones there is no room for in a chord.
(define-leader "SPC a a" "ai")
(define-leader "SPC a n" "terminal-next")
(define-leader "SPC a p" "terminal-prev")
(define-leader "SPC a r" "terminal-restart")
(define-leader "SPC a k" "terminal-close")
(define-leader "SPC a t" "terminal-new")
(define-leader "SPC a f" "ai-find-file-at-point")

;;; The session verbs are *editor* verbs, not Lisp functions, so introspection
;;; cannot find them for `M-x' the way it finds everything else in this file.
;;; `*extra-commands*' is the list for exactly that case, and `refresh-commands'
;;; at the end of init.lisp is what publishes it — which is why this pushes
;;; rather than registering directly.
(dolist (verb '("terminal-new" "terminal-next" "terminal-prev"
                "terminal-restart" "terminal-close"))
  (pushnew verb *extra-commands* :test #'string=))
