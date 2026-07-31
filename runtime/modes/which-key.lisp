;;;; which-key, and the annotation `M-x' shows beside a command.
;;;;
;;;; Two features, one question: *what is this called, and where is it bound*.
;;;; Both are answered out of `key-bindings', which is a reader — so nothing
;;;; here keeps a keymap of its own. A shadow table filled in as `define-key'
;;;; is called would go stale the first time anything bound a key without
;;;; coming through this file; the editor's own map cannot.
;;;;
;;;; which-key needs one fact Lisp cannot see for itself: that a *prefix* has
;;;; been pressed and the editor is waiting for the rest of the sequence. That
;;;; is three lines in `normal_key' (crates/core/src/evil.rs), which hand the
;;;; pending sequence over as `(which-key "SPC f")' and guard the call with
;;;; `fboundp', so a config that never loads this file pays nothing at all.
;;;; Everything a user sees is below, in Lisp, where it can be bent.
;;;;
;;;; The M-x annotation is the same table read the other way round. Rather than
;;;; teach the Rust minibuffer about a second column, a candidate carries its
;;;; annotation inside a `#| |#' block comment — so the candidate is still the
;;;; *source of a legal call*, which is exactly what core does with it:
;;;; `run_action' hands anything it does not recognise to the image as
;;;; `(candidate)', and a block comment reads as nothing. Marginalia for one
;;;; `format', and not a line of Rust.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; which-key

(defparameter *which-key-limit* 18
  "How many continuations to show before saying how many were left out. The
status line is one line: past this many it is unreadable anyway, and letting
the renderer truncate would cut a key in half rather than admit there is more.")

(defparameter *which-key-groups*
  '(("SPC b" . "buffer") ("SPC f" . "file")    ("SPC g" . "git")
    ("SPC h" . "help")   ("SPC j" . "jump")    ("SPC m" . "mode")
    ("SPC o" . "open")   ("SPC p" . "project") ("SPC q" . "quit")
    ("SPC s" . "search") ("SPC t" . "toggle")  ("SPC w" . "window")
    ("C-c"   . "eval")   ("C-x"   . "eval")    ("SPC"   . "leader"))
  "What to call a prefix that only leads to further prefixes. Emacs' which-key
writes `+file'; an unnamed group comes out as `+...', which is still the useful
half of the answer — that there is something under this key.")

(defun %key-tokens (keys)
  "KEYS split on spaces. `key-bindings' hands out the normalised spelling, so
this is a split and never a parse."
  (let ((out nil) (i 0) (n (length keys)))
    (loop while (< i n)
          do (let ((j (or (position #\Space keys :start i) n)))
               (when (> j i) (push (subseq keys i j) out))
               (setf i (1+ j))))
    (nreverse out)))

(defun %key-continuation (prefix keys)
  "The single token by which KEYS continues PREFIX, or NIL when it does not.
Second value: true when KEYS *ends* there, so the row names a command rather
than another prefix."
  (let* ((p (%key-tokens prefix))
         (k (%key-tokens keys))
         (n (length p)))
    (when (and (> (length k) n) (every #'string= p (subseq k 0 n)))
      (values (nth n k) (= (length k) (1+ n))))))

(defun %which-key-group (prefix token)
  (let ((named (cdr (assoc (concatenate 'string prefix " " token)
                           *which-key-groups* :test #'string=))))
    (concatenate 'string "+" (or named "..."))))

(defun %which-key-maps ()
  "The keymaps that apply to the live buffer, in the order the editor consults
them: minor modes most-recently-enabled first, then the major mode, then the
modal state's own map. The order is the point — the nearest map holds the
binding that will actually fire, so it is the one whose label must win."
  (append (reverse (minor-modes)) (list (major-mode) (evil-state))))

(defun which-key-rows (prefix)
  "(TOKEN . LABEL) for every key that continues PREFIX in the live buffer, each
token once, nearest keymap first, sorted by key."
  (let ((rows nil)
        (bindings (key-bindings)))       ; one reader call, not one per row
    (dolist (map (%which-key-maps))
      (dolist (b bindings)
        (when (string= (first b) map)
          (multiple-value-bind (token wholep) (%key-continuation prefix (second b))
            (when (and token (not (assoc token rows :test #'string=)))
              (push (cons token (if wholep (third b) (%which-key-group prefix token)))
                    rows))))))
    (sort (nreverse rows) #'string< :key #'car)))

(defun which-key (prefix)
  "Show what continues PREFIX, in the status line.

Called by the editor the moment a prefix key is pressed — that is the whole
feature — and by hand from `M-x' or from `which-key-leader' below.

ponytail: one line, because `message' is the only surface Lisp can draw on. A
real which-key *panel* wants overlays, which live in the renderer; when they
arrive this function is the only thing that has to change.

ponytail: this is the one thing in the editor that runs Lisp per *keystroke*
rather than per command — every key of a leader sequence but the last. It costs
one `key-bindings' read and a walk over it, on the Lisp thread, where nothing
is waiting for it. If that ever shows up, the answer is a cache invalidated by
wrapping `define-key', not a table filled in by hand."
  (let* ((rows (which-key-rows prefix))
         (n (length rows))
         (shown (subseq rows 0 (min n *which-key-limit*))))
    (when rows
      (message
       (format nil "~a-  ~{~a~^   ~}~@[   +~a more~]"
               prefix
               (mapcar (lambda (row) (format nil "~a ~a" (car row) (cdr row))) shown)
               (when (> n *which-key-limit*) (- n *which-key-limit*)))))))

(defun which-key-leader ()
  "Show the leader bindings without waiting for the leader key."
  (which-key "SPC"))

;;; ---------------------------------------------------------------------------
;;; M-x annotations
;;;
;;; `(documentation 'foo 'function)' is half of it and `where-is' is the other
;;; half; the only question was how to get two more columns past a prompt that
;;; draws one string per candidate. The answer is that the candidate is *Lisp
;;; source* — core runs an unrecognised one as `(candidate)' — so an annotation
;;; that reads as nothing rides along inside it for free.

(defparameter *annotation-column* 24
  "Where the annotation starts. Padding rather than a column, because the
prompt draws one string per candidate and knows nothing about columns.")

(defparameter *core-annotated-commands*
  '("quit" "new-frame" "delete-frame" "delete-window" "other-window"
    "split-window-right" "split-window-below")
  "Names `BUILTIN_COMMANDS' (crates/core/src/evil.rs) already offers M-x and
which this image also defines as functions. Core lists its own first and drops
a duplicate by *exact* match — which an annotated spelling would slip straight
past — so these stay bare and core goes on deduplicating them as it always did.")

(defun %first-line (s)
  "S up to its first newline. A docstring is a paragraph; a candidate is a row."
  (let ((nl (position #\Newline s)))
    (if nl (subseq s 0 nl) s)))

(defun %comment-safe (s)
  "S with the one character that could close the block comment early taken out.
A `|' in a docstring is rare; a candidate that stops reading half way through
is not the kind of thing to leave to luck."
  (remove #\| s))

(defun command-annotation (name)
  "NAME's docstring and the key it is bound to, as one line, or NIL when it has
neither and there is nothing worth saying."
  (let* ((symbol (find-symbol (string-upcase name) :zemacs))
         (doc (and symbol (fboundp symbol)
                   (ignore-errors (documentation symbol 'function))))
         (key (second (first (where-is name)))))
    (cond ((and doc key) (format nil "~a   ~a" (%comment-safe (%first-line doc)) key))
          (doc (%comment-safe (%first-line doc)))
          (key key))))

(defun %annotated-command (name)
  "NAME as M-x should list it: the name, then its annotation inside a Lisp
block comment. `(name #| doc  KEY |#)' is a legal call to NAME, which is what
core will make of the candidate, so the annotation costs the minibuffer nothing
and the command still runs."
  (let ((note (and (not (member name *core-annotated-commands* :test #'string=))
                   (command-annotation name))))
    (if note
        (format nil "~va #| ~a |#" *annotation-column* name note)
        name)))

;;; ponytail: core resolves `magit-', `dired-', `project-', `terminal-' and
;;; `-mode' by *prefix* before it falls through to the image, and an annotated
;;; candidate no longer looks like any of them. Nothing shipped is affected —
;;; those verbs are core's own and are published by core, unannotated — but a
;;; config defining a Lisp `project-foo' would find `M-x' running it as a Lisp
;;; call rather than as a project verb, which is what it wanted anyway. The fix,
;;; if it ever bites, is one `split` in `accept_prompt'.

(defvar *raw-register-command* (fdefinition 'register-command)
  "`register-command' as the shim defined it. DEFVAR and not DEFPARAMETER: a
config reload re-reads this file, and re-capturing would store the wrapper as
the original and annotate the annotation.")

(defun register-command (name)
  "Offer NAME in M-x, with its docstring and its key beside it.

Wrapping the primitive rather than rewriting `refresh-commands' means every
route into the M-x list is covered at once — `refresh-commands', the
`register-command' that `define-derived-mode' issues, and a config's own call."
  (funcall *raw-register-command*
           (%annotated-command (string-downcase (string name)))))
