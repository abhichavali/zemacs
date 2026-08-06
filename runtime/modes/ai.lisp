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
;;; Arguments are split the way a shell splits them on the way through the verb
;;; — see `words' in `crates/app/src/term.rs' — so a flag whose value contains a
;;; space is written the way you would write it at a prompt, in quotes. That is
;;; what `ai-send-prompt' below relies on, and it is the note that used to stand
;;; here coming due.

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
;;;
;;; `executable-find' itself was written here first, and lives in `modes.lisp'
;;; now: `tutor.lisp' and `math-written.lisp' both wanted it and both reached it
;;; through an `(fboundp 'executable-find)' guard, because neither could be sure
;;; this file had loaded. A helper in the file every mode loads first needs no
;;; such guard.

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
  "Fork HARNESS beside the current window. RESUME picks its argument lists."
  (if (null (executable-find (second harness)))
      ;; The editor refuses this too, before it forks. Saying it here as well
      ;; means the answer comes from the thing you picked rather than from a
      ;; buffer that appeared and vanished.
      (message (format nil "~a is not installed — no ~a on $PATH"
                       (first harness) (second harness)))
      ;; Side by side, because the whole point of an agent is reading what it
      ;; says against the code it is saying it about — taking over the window
      ;; you were reading is the one layout that cannot do that.
      ;; `split-window-right' focuses the new pane and the verb travels the
      ;; same queue behind it, so the session lands there.
      (progn (split-window-right)
             (call-command (%ai-verb harness resume)))))

;;; ---------------------------------------------------------------------------
;;; One question, no session
;;;
;;; A harness on a PTY is the right shape for a conversation and the wrong one
;;; for a *question*: starting a full-screen TUI, waiting for it to come up and
;;; reading one paragraph out of it is a lot of ceremony for "what does this
;;; flag do". `claude -p' is the one-shot form — it prints an answer and exits —
;;; and it is still a session buffer, because that is where a subprocess's
;;; output goes here and because you want to scroll it.
;;;
;;; `rerun:' rather than `run:', which is the whole of the difference between
;;; this and picking `claude' from the menu: a question is a thing you ask
;;; *again*, so the buffer is reused. Twenty one-shot answers would otherwise be
;;; twenty dead sessions in the switcher inside a minute.

(defparameter *ai-prompt-harness* "claude"
  "Which harness `ai-send-prompt' asks. Its program is looked up in
`*ai-harnesses*', so pointing this at another entry is all it takes.")

(defparameter *ai-prompt-flag* "-p"
  "The one-shot flag. `claude -p PROMPT' prints an answer and exits; the other
two spell it differently, which is why this is a variable and not a literal.")

(defun %ai-quote (string)
  "STRING as one shell-style argument: wrapped in double quotes, with the two
characters that would end it escaped.

The verb's splitter (`words' in `crates/app/src/term.rs') understands exactly
this, and nothing is handed to a shell — so a backtick, a `$' or a semicolon in
a prompt is text, and quoting is only about where the argument ends."
  (with-output-to-string (out)
    (write-char #\" out)
    (loop for ch across string
          do (when (or (char= ch #\") (char= ch #\\)) (write-char #\\ out))
             (write-char ch out))
    (write-char #\" out)))

(defun ai-send-prompt ()
  "Ask the agent one question, typed in the minibuffer, and show the answer.

For the things that are not worth a conversation: what a flag does, what a
stack trace means, a one-line rewrite. `M-x ai-send-prompt', or `send prompt'
in the `C-a' menu."
  (let ((harness (assoc *ai-prompt-harness* *ai-harnesses* :test #'string=)))
    (cond
      ((null harness)
       (message (format nil "no such agent: ~a" *ai-prompt-harness*)))
      ((null (executable-find (second harness)))
       (message (format nil "~a is not installed — no ~a on $PATH"
                        (first harness) (second harness))))
      (t
       (read-string "Prompt: "
         (lambda (prompt)
           ;; Cancelled, or entered empty. Neither is an error and neither is a
           ;; question, so neither forks anything.
           (when (and prompt (plusp (length (string-trim " " prompt))))
             (split-window-right)
             (call-command
              (format nil "terminal-rerun:~a-p:~a ~a ~a"
                      (first harness) (second harness) *ai-prompt-flag*
                      (%ai-quote prompt))))))))))

(defparameter *ai-send-prompt-label* "send prompt"
  "What `ai-send-prompt' is called in the menu. Not a harness, so it is matched
by name before the harness list is searched.")

(defun ai ()
  "Pick a coding agent, then start a new conversation or resume one.
`send prompt' is the odd one out: one question, one answer, no session.
Bound to `C-a'."
  (completing-read "Agent: " (append (mapcar #'%ai-label *ai-harnesses*)
                                     (list *ai-send-prompt-label*))
    ;; A `cond' and not a `return-from': this callback runs on a *later* turn of
    ;; the Lisp queue (threading.org), by which time the block `ai' established
    ;; has long since exited and jumping to it would be a control error rather
    ;; than an early return.
    (lambda (pick)
      (cond
        ((null pick))
        ((equal pick *ai-send-prompt-label*) (ai-send-prompt))
        (t
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
                     (%ai-start harness (string= what "resume"))))))))))))

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
(define-leader "SPC a s" "ai-send-prompt")

;;; The session verbs are *editor* verbs, not Lisp functions, so introspection
;;; cannot find them for `M-x' the way it finds everything else in this file.
;;; `*extra-commands*' is the list for exactly that case, and `refresh-commands'
;;; at the end of init.lisp is what publishes it — which is why this pushes
;;; rather than registering directly.
(dolist (verb '("terminal-new" "terminal-next" "terminal-prev"
                "terminal-restart" "terminal-close"))
  (pushnew verb *extra-commands* :test #'string=))
