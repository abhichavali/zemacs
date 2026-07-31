;;;; org-modern and org-appear — org's markup *drawn* rather than typed.
;;;;
;;;; An overlay carrying a `display' string is drawn instead of the characters
;;;; it covers, and the renderer substitutes cells rather than painting over
;;;; them — so `***' really does become one bullet and the heading text really
;;;; does stay where it was. That is the whole mechanism. Everything below is
;;;; policy: which ranges, which glyph, and when to stop hiding.
;;;;
;;;; `org-latex-preview' in `init.lisp' is the model, and this is the same shape
;;;; one size up: a Lisp-side property (`:org-modern') marks an overlay as ours,
;;;; so a refresh replaces exactly its own overlays and leaves the LaTeX images,
;;;; an avy hint or anything else alone. `remove-overlays' is the blunt
;;;; instrument and is never used here.
;;;;
;;;; org-appear is the other half: an overlay hides markup, so *deleting its
;;;; display* reveals it, and the question "which one is the cursor in" is one
;;;; `overlays-at'. Re-hiding re-reads the text underneath and renders it again
;;;; rather than restoring a string saved when the overlay was made — which is
;;;; what keeps typing *inside* a revealed fragment from putting a stale glyph
;;;; back over it afterwards.
;;;;
;;;; ponytail — the scanner is here, in Lisp, and it should not be.
;;;; `crates/syntax/src/org.rs' already has `bullets()', which answers exactly
;;;; the heading/list question with the level and the marker range, and it is
;;;; *not reachable from the image*: there is no arm for it in `query.rs' and no
;;;; name for it in `QUERIES_FORM'. `latex-fragments' is the precedent — one
;;;; reader, answered outside `query.rs' because org's scanner is downstream of
;;;; core — and an `org-bullets' beside it would delete `%org-modern-line' and
;;;; half of `%org-modern-scan' below. Until then the classification is repeated
;;;; here and can drift from the highlighter's; the two agree today, and the
;;;; places they must agree are marked.
;;;;
;;;; The other ceiling, and the one you will feel: there is no cursor-movement
;;;; hook. `after-change-hook' is the only thing the editor reports about a
;;;; buffer, so `org-modern-appear' runs when you *type* and not when you merely
;;;; move — see the note above the PUSHNEW near the foot of this file for what
;;;; that costs and what closes it.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; What gets drawn instead of what
;;;
;;; Five tables, all `defparameter' so a config can set them and re-run
;;; `org-modern-refresh' without reloading anything.

(defparameter *org-modern-stars* '("◉" "○" "✸" "✿" "❀" "✜")
  "One bullet per heading level, cycling past the end.

A level-N heading substitutes N-1 spaces and then its bullet for its N stars,
so the glyph steps right as the level deepens and the heading *text* does not
move at all — the substitution is the same width as what it replaces.")

(defparameter *org-modern-list-bullets* '((#\- . "•") (#\+ . "◦"))
  "One glyph per list marker. `1.' and `1)' are deliberately absent, for the
reason `crates/syntax' gives: a number is not one character and has no glyph to
become.")

(defparameter *org-modern-checkboxes*
  '((#\Space . "☐") (#\X . "☑") (#\x . "☑") (#\- . "☒"))
  "One glyph per `[ ]' cookie, keyed by the character between the brackets.
Three cells become one, so the item text does shift left — which is what
org-modern does too, and what makes a checkbox read as one thing.")

(defparameter *org-modern-emphasis*
  '((#\* . "bold") (#\/ . "italic") (#\_ . "italic")
    (#\= . "code") (#\~ . "code") (#\+ . "comment"))
  "Emphasis marker -> the face its body is drawn in.

The same mapping `marker_kind' in `crates/syntax/src/org.rs' makes, and it has
to be: the overlay covers the *whole* run, markers included, so the display
string inherits the highlight at the run's first character — which is the
markup face of the marker — unless the overlay claims a face of its own.")

;;; ---------------------------------------------------------------------------
;;; Reading a line of org
;;;
;;; Line-oriented, because org is: it decides what a line *is* from its first
;;; few characters, and only `#+begin_'/`#+end_' blocks carry state across
;;; lines. One pass, one line at a time, exactly as `org.rs' does it.

(defun %org-blank-p (c) (member c '(#\Space #\Tab #\Return)))

(defun %org-directive-p (line word)
  "True when LINE, after its indentation, starts with WORD, ignoring case."
  (let* ((n (length line))
         (i (or (position-if-not #'%org-blank-p line) n))
         (m (length word)))
    (and (<= (+ i m) n) (string-equal word line :start2 i :end2 (+ i m)))))

(defun %org-border-p (c)
  "Org's BORDER: an emphasis body may not begin or end with whitespace, nor
with a comma."
  (and (not (%org-blank-p c)) (char/= c #\,)))

(defparameter *org-emphasis-pre* '(#\- #\( #\' #\" #\{)
  "What may sit immediately before an emphasis opener, besides whitespace and
the start of the line.")

(defparameter *org-emphasis-post* '(#\- #\. #\, #\: #\! #\? #\; #\' #\" #\) #\} #\[)
  "What may sit immediately after an emphasis closer, besides whitespace and
the end of the line.")

(defun %org-emphasis-at (line i)
  "Index of the marker closing an emphasis run that opens at I, or NIL.

Org's rule is a triple — the character before the opener, the two borders of
the body, the character after the closer — and all three are needed. Without
the first, `http://x/a/b/' is italics; without the second, `2 * 3' is bold."
  (let ((n (length line)) (c (char line i)))
    (when (and (assoc c *org-modern-emphasis*)
               (or (zerop i)
                   (%org-blank-p (char line (1- i)))
                   (member (char line (1- i)) *org-emphasis-pre*))
               (< (1+ i) n)
               (char/= (char line (1+ i)) c)
               (%org-border-p (char line (1+ i))))
      (loop for j from (+ i 2) below n
            when (and (char= (char line j) c)
                      (%org-border-p (char line (1- j)))
                      (or (= (1+ j) n)
                          (%org-blank-p (char line (1+ j)))
                          (member (char line (1+ j)) *org-emphasis-post*)))
              do (return j)))))

(defun %org-link-at (line i)
  "Index just past the `]]' of a `[[target]]' or `[[target][text]]' at I, or NIL."
  (let ((n (length line)))
    (when (and (char= (char line i) #\[)
               (< (1+ i) n)
               (char= (char line (1+ i)) #\[))
      (let ((close (loop for j from (+ i 2) below (1- n)
                         when (and (char= (char line j) #\])
                                   (char= (char line (1+ j)) #\]))
                           do (return j))))
        ;; `[[]]' links to nothing and is left as the four characters it is.
        (when (and close (> close (+ i 2))) (+ close 2))))))

(defun %org-checkbox-at (line from)
  "(BEG . END) of a `[ ]', `[X]' or `[-]' cookie at or just after FROM, or NIL."
  (let* ((n (length line))
         (i (or (position-if-not #'%org-blank-p line :start (min from n)) n)))
    (when (and (< (+ i 2) n)
               (char= (char line i) #\[)
               (char= (char line (+ i 2)) #\])
               (assoc (char line (1+ i)) *org-modern-checkboxes*))
      (cons i (+ i 3)))))

(defun %org-modern-inline (line from)
  "The links and emphasis runs in LINE at or after FROM, as (BEG END KIND).

Markup does not nest, so a hit skips its whole body — which is also what stops
the closing marker of one run from opening the next."
  (let ((n (length line)) (out nil) (i from))
    (loop while (< i n)
          do (let ((link (%org-link-at line i)))
               (cond (link (push (list i link :link) out) (setf i link))
                     (t (let ((close (%org-emphasis-at line i)))
                          (cond (close (push (list i (1+ close) :emphasis) out)
                                       (setf i (1+ close)))
                                (t (incf i))))))))
    (nreverse out)))

(defun %org-modern-line (line)
  "The substitutions LINE asks for, as (BEG END KIND) in offsets into LINE.

The three lines that are *not* prose — a `#+directive', a `# comment' and a
`|table|' row — are skipped whole, which is the same decision `org.rs' makes
about them and for the same reason: a rule like `|--+--|' would otherwise read
as a `+strike+' run.

A heading is skipped whole too, and that one is a rule about agreement rather
than about taste: the highlighter paints a heading line in one face, emphasis
included, so substituting inside one would be the only place two things claimed
the same cells."
  (let* ((n (length line))
         (indent (or (position-if-not #'%org-blank-p line) n))
         (head (and (< indent n) (char line indent)))
         (stars (and (zerop indent) (eql head #\*)
                     (let ((k (or (position-if-not (lambda (c) (char= c #\*)) line) n)))
                       (and (< k n) (char= (char line k) #\Space) k)))))
    (cond
      ((null head) nil)
      (stars (list (list 0 stars :heading)))
      ((member head '(#\# #\|)) nil)
      ;; `- item' / `+ item', then its checkbox, then the prose after both.
      ((and (member head '(#\- #\+))
            (< (1+ indent) n)
            (%org-blank-p (char line (1+ indent))))
       (let* ((box (%org-checkbox-at line (+ indent 2)))
              (from (if box (cdr box) (+ indent 2))))
         (append (list (list indent (1+ indent) :list))
                 (when box (list (list (car box) (cdr box) :checkbox)))
                 (%org-modern-inline line from))))
      (t (%org-modern-inline line indent)))))

(defun %org-modern-scan (text)
  "Every substitution TEXT wants, as (BEG END KIND LITERAL): BEG and END in
*character* offsets — which is what `make-overlay' takes — and LITERAL the text
between them, which is what `%org-modern-render' turns into a glyph.

Two offset spaces, and both are needed. `buffer-string' hands out UTF-8 bytes,
every offset the editor takes is a character, and `%char-index' converts. It is
called per *line* rather than per hit, because it counts continuation bytes from
the start of whatever it is given — over the whole buffer that would be
quadratic, and over a line it is a few dozen characters."
  (let ((n (length text)) (out nil) (in-block nil) (bol 0) (cbol 0))
    (loop while (<= bol n)
          do (let* ((eol (or (position #\Newline text :start bol) n))
                    (line (subseq text bol eol)))
               ;; A `- ' inside `#+begin_src' is a diff line or a shell flag,
               ;; and a `*' in one is a glob. An unterminated block swallows the
               ;; rest of the file, which beats guessing.
               (cond ((%org-directive-p line "#+begin_") (setf in-block t))
                     ((%org-directive-p line "#+end_") (setf in-block nil))
                     (in-block)
                     (t (dolist (s (%org-modern-line line))
                          (destructuring-bind (a b kind) s
                            (push (list (+ cbol (%char-index line a))
                                        (+ cbol (%char-index line b))
                                        kind
                                        (subseq line a b))
                                  out)))))
               (setf bol (1+ eol)
                     cbol (+ cbol (%char-index line (length line)) 1))))
    (nreverse out)))

;;; ---------------------------------------------------------------------------
;;; What a range is drawn as
;;;
;;; One function, used twice: the scan calls it to make an overlay, and
;;; org-appear calls it to re-hide one. That is deliberate — it means a revealed
;;; fragment can be *edited* while it is revealed and still hide correctly
;;; afterwards, because nothing was cached.

(defun %org-modern-render (kind text)
  "(DISPLAY . FACE) for the literal TEXT of a KIND substitution, or NIL when
TEXT is no longer that kind of markup at all.

NIL is the useful answer: an overlay whose text was edited under it retires
itself instead of drawing a glyph over something that is not there any more.

FACE is NIL wherever the syntax highlighter already has the colour right — a
display string is attributed to the first character it covers, so it inherits
that character's highlight for free. Only emphasis and links need to say
otherwise, because the character they start on is a markup marker."
  (let ((n (length text)))
    (case kind
      (:heading
       (when (and (plusp n) (every (lambda (c) (char= c #\*)) text))
         (cons (concatenate 'string
                            (make-string (1- n) :initial-element #\Space)
                            (nth (mod (1- n) (length *org-modern-stars*))
                                 *org-modern-stars*))
               nil)))
      (:list
       (let ((glyph (and (= n 1) (cdr (assoc (char text 0) *org-modern-list-bullets*)))))
         (when glyph (cons glyph nil))))
      (:checkbox
       (let ((glyph (and (= n 3)
                         (char= (char text 0) #\[)
                         (char= (char text 2) #\])
                         (cdr (assoc (char text 1) *org-modern-checkboxes*)))))
         (when glyph (cons glyph nil))))
      (:emphasis
       (let ((face (and (> n 2) (cdr (assoc (char text 0) *org-modern-emphasis*)))))
         (when (and face (char= (char text 0) (char text (1- n))))
           (cons (subseq text 1 (1- n)) face))))
      (:link
       (when (and (> n 4)
                  (string= "[[" text :end2 2)
                  (string= "]]" text :start2 (- n 2)))
         (let* ((body (subseq text 2 (- n 2)))
                (split (search "][" body)))
           (cons (if split (subseq body (+ split 2)) body) "link")))))))

;;; ---------------------------------------------------------------------------
;;; The overlays
;;;
;;; `:org-modern' holds the *kind*, which is both the mark saying "this one is
;;; mine" and everything `%org-modern-render' needs to draw it again. One
;;; property, two jobs.

(defun %org-modern-overlays (&optional (beg (point-min)) (end (point-max)))
  "Handles of the overlays this file made, overlapping BEG..END."
  (remove-if-not (lambda (o) (overlay-get o :org-modern))
                 (mapcar #'first (overlays-in beg end))))

(defvar *org-modern-revealed* nil
  "The one overlay currently showing its literal markup, or NIL.

DEFVAR rather than DEFPARAMETER: reloading the config must not forget that
something on screen is revealed, or the next `org-modern-appear' would leave it
revealed forever.")

(defun %org-modern-remove ()
  "Drop every overlay this file made, and answer how many there were.

Exactly its own: `remove-overlays' would take the LaTeX previews with them,
which is what the `:org-modern' mark exists to prevent."
  (setf *org-modern-revealed* nil)
  (let ((ovs (%org-modern-overlays)))
    (mapc #'delete-overlay ovs)
    (length ovs)))

(defun org-modern-clear ()
  "Take every substitution off, showing org's punctuation again."
  (message (format nil "~d substitution~:p cleared" (%org-modern-remove))))

(defun org-modern-refresh ()
  "Redraw every substitution in the buffer.

Its own overlays first, so this doubles as `refresh' and is safe to call as
often as you like. One scan of `buffer-string' and one overlay per hit — which
is why it is a *command* and not something on `after-change-hook' — see the
note beside that hook at the foot of this file."
  (%org-modern-remove)
  (let ((made 0))
    (dolist (s (%org-modern-scan (buffer-string)))
      (destructuring-bind (beg end kind text) s
        (let ((look (%org-modern-render kind text)))
          (when look
            (let ((ov (make-overlay beg end)))
              (when ov
                (overlay-put ov :org-modern kind)
                (when (cdr look) (overlay-put ov 'face (cdr look)))
                (overlay-put ov 'display (car look))
                (incf made)))))))
    ;; Whatever the cursor is already sitting in should not have been hidden.
    (org-modern-appear)
    ;; Ends on the message, so it answers NIL: `eval-string' echoes the value of
    ;; the last form and would otherwise wipe out what this just said.
    (message (format nil "~d substitution~:p" made))))

(defun %org-modern-rehide (ov)
  "Put OV's glyph back, re-reading the text it covers."
  (when ov
    (let* ((at (overlay-position ov))
           (look (and at (%org-modern-render (overlay-get ov :org-modern)
                                             (buffer-substring (car at) (cdr at))))))
      (cond
        ;; NIL from `overlay-position' means gone *or* in another buffer, and
        ;; nothing tells the two apart — so this forgets it rather than deleting
        ;; it. Left revealed in a buffer you are not looking at, an overlay
        ;; shows the markup it covers, which is what the mode being off looks
        ;; like anyway; deleting somebody else's buffer's overlay is not.
        ((null at))
        (look (when (cdr look) (overlay-put ov 'face (cdr look)))
              (overlay-put ov 'display (car look)))
        ;; The text under it is not the markup it was made for any more — an
        ;; overlay that cannot draw itself has nothing to say.
        (t (delete-overlay ov))))))

(defun org-modern-appear ()
  "Reveal the literal markup the cursor is inside, and hide everything else.

org-appear, in a dozen lines and no state worth the name: at most one overlay
is revealed at a time, so the work is `is the cursor still in the one I
revealed' and, when it is not, two `overlay-put's. That is what makes this
affordable on every keystroke where a full rescan is not."
  (when (minor-mode-p 'org-modern)
    (let ((now (first (%org-modern-overlays (point) (1+ (point))))))
      (unless (eql now *org-modern-revealed*)
        (%org-modern-rehide *org-modern-revealed*)
        ;; NIL clears the property, so the characters underneath are drawn —
        ;; which is the whole of "reveal".
        (when now (overlay-put now 'display nil))
        (setf *org-modern-revealed* now))))
  ;; Answers NIL rather than an overlay handle: this is a command as well as a
  ;; hook, and `M-x' echoing an integer at you says nothing.
  nil)

;;; ---------------------------------------------------------------------------
;;; The minor mode, and the one hook there is

(define-minor-mode org-modern
  "Draw org's markup instead of its punctuation: heading stars become bullets,
`- ' becomes `•', `[X]' becomes `☑', `[[a][b]]' becomes `b', and `*bold*'
becomes bold text with its asterisks hidden until the cursor is inside it.

Turning it off removes exactly the overlays it made."
  (:on (org-modern-refresh))
  (:off (%org-modern-remove)))

;;; `after-change-hook' is defined in `lsp.lisp', which `init.lisp' loads first
;;; and inside a `handler-case' — so a machine with no `rpc.lisp' would leave
;;; this file with nowhere to hang. Both forms are no-ops when that file did
;;; load: DEFVAR does not overwrite a bound variable, and the function is only
;;; defined when there is not one already.
;;;
;;; ponytail: this hook, and `*after-change-functions*' with it, belongs in the
;;; standard library beside `define-derived-mode'. It is in the LSP client
;;; because the LSP client was the only thing that wanted it, and this file is
;;; the second — which is the day it should move.
(defvar *after-change-functions* nil
  "Functions called with no arguments after any change to the live buffer.")

(unless (fboundp 'after-change-hook)
  (defun after-change-hook ()
    (dolist (f *after-change-functions*) (ignore-errors (funcall f)))))

;;; Hanging org-appear off the one event the editor reports about a buffer.
;;;
;;; ponytail — and this is the ceiling you will actually feel.
;;; `after-change-hook' fires when the *document* moves, not when the *cursor*
;;; does, so markup is revealed as you type and not as you navigate: in Normal
;;; mode `j' and `w' change nothing, so nothing fires, and the asterisks around
;;; the word you just moved onto only appear once you press `i' and type the
;;; first character. `SPC m a' asks for it by hand in the meantime.
;;;
;;; The upgrade is one line in the application, in exactly the place
;;; `after-change-hook' is pushed from — a `point-moved-hook' queued when the
;;; cursor offset changes, taking the same route through `pending_hooks' and the
;;; same `fboundp' guard. Nothing here would change but the name in the PUSHNEW.
;;;
;;; Rebinding the motion keys to Lisp wrappers is the other way, and it is the
;;; wrong one: it would reimplement counts, operators and the desired column in
;;; the image to buy a hook, which is the trade the boundary exists to refuse.
;;;
;;; Note what is *not* hung here: `org-modern-refresh'. A rescan is one query
;;; plus an overlay per hit, and doing that per keystroke would put a `%do' per
;;; bullet between you and your next character. So markup typed since the last
;;; refresh has no glyph until you ask for one — `SPC m m', or leaving and
;;; re-entering the mode. Closing that properly wants the change *delta*
;;; `boundary.org' lists as missing, which would let this rescan the two lines
;;; that moved instead of all of them.
(pushnew 'org-modern-appear *after-change-functions*)

;;; ---------------------------------------------------------------------------
;;; Keys, and turning it on
;;;
;;; `org-mode' is declared in `library.lisp' with an empty body; this re-declares
;;; it with one, which is how the mode system is meant to be bent — the tables
;;; `define-derived-mode' writes are `*mode-parents*' and `*mode-bodies*', and
;;; the `set-mode-local' claim on `relative-line-numbers' lives in a third table
;;; and survives untouched. Re-declaring rather than wrapping is what makes a
;;; config reload idempotent instead of stacking a wrapper per reload.
;;;
;;; Guarded by `minor-mode-p' because the body runs on every entry into the
;;; mode, and `org-modern' is a toggle.

(define-derived-mode org-mode text-mode
  (unless (minor-mode-p 'org-modern) (org-modern)))

(define-key "org-mode" "SPC m m" "org-modern-refresh")
(define-key "org-mode" "SPC m M" "org-modern")
(define-key "org-mode" "SPC m a" "org-modern-appear")
