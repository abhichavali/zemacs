;;;; org-agenda.lisp — every unfinished thing, in one list.
;;;;
;;;; org's agenda is a large feature and this is the half of it anybody uses
;;;; daily: *where are my TODOs*. It is here rather than unbuilt because the
;;;; thing it needed was built for something else — `runtime/modes/xref.lisp' is
;;;; a buffer of `PATH:LINE:TEXT' rows where `RET' opens the place named on the
;;;; line, and a list of headlines is exactly that shape. So this file is a
;;;; scanner and nothing more; the listing, the keymap and the jump are already
;;;; written.
;;;;
;;;; What it is *not* is a calendar. There is no clock here, no `SCHEDULED:'
;;;; sort, no day view — those want timestamp parsing and a sense of today, and
;;;; each is a feature of its own rather than a line in this one. What is here
;;;; is the query org users run twenty times a day and the two obvious filters
;;;; over it.
;;;;
;;;;   (org-todo-list)     everything not DONE, across the agenda files
;;;;   (org-agenda-tags)   the same, narrowed to one tag
;;;;   *org-agenda-files*  where to look; NIL means the buffer you are in
;;;;
;;;; The one ordering that *is* here is priority — `[#A]' first — because it
;;;; needs no clock and no notion of today, only a letter already written on the
;;;; headline. A `stable-sort', so everything else stays in outline order.

(in-package :zemacs)

(defparameter *org-agenda-files* nil
  "Files the agenda scans, as paths. NIL means *the buffer you are in*, which is
the useful default for a project that keeps one `notes.org' and the only one
that needs no configuration at all.

A list makes it org's own model:

  (setf *org-agenda-files* (list \"~/org/work.org\" \"~/org/home.org\"))

A directory is not walked. That is deliberate rather than missing: a walk needs
a rule about what is in and what is out, and a list you wrote is a rule you can
read. `(directory \"~/org/*.org\")' in your init is the walk, spelled where you
can see it.

`~/' is expanded, because Common Lisp does not do it and the example above is
the spelling everybody writes. One bare string is taken as a list of one, for
the same reason: it is the other thing everybody writes.")

(defparameter *org-agenda-done* '("DONE" "CANCELLED" "CANCELED")
  "Keywords that mean *finished*, and are therefore left out of the list.

Separate from `*org-todo-keywords*' — which is the cycle, in order — because
which of those states counts as done is a different question from which exist. A
config adding `WAIT' to the cycle wants it in the agenda; one adding `CANCELLED'
does not.

Both spellings of cancelled are here because both are in the wild — org ships
the doubled `L' and half the configs on the internet do not — and a word nobody
has put in `*org-todo-keywords*' can never be read as a keyword anyway, so the
one that is wrong for you costs nothing.")

(defun %org-agenda-heading (line)
  "(STARS KEYWORD TEXT) for an org headline, or NIL if LINE is not one.

The keyword is only a keyword when it is a word from `*org-todo-keywords*' — the
same rule `org-modern.lisp' parses by, so a headline reading `Doneness of the
proof' is not a DONE item."
  (let ((n 0))
    (loop while (and (< n (length line)) (char= (char line n) #\*)) do (incf n))
    ;; A headline is stars *and then a space*: `**bold**' at the start of a line
    ;; is emphasis, not a level-two heading.
    (when (and (plusp n) (< n (length line)) (char= (char line n) #\Space))
      (let* ((rest (string-left-trim " " (subseq line n)))
             (space (position #\Space rest))
             (word (if space (subseq rest 0 space) rest)))
        (if (member word *org-todo-keywords* :test #'string=)
            (list n word (string-left-trim " " (if space (subseq rest space) "")))
            (list n nil rest))))))

(defun %org-agenda-tags (text)
  "The tags on a headline's text — `:work:urgent:' at its end — as a list.

Org's own shape: one colon-delimited run, at the end of the line, no spaces in
it. So the test is on the *last word*, which is what makes `Ship it:' and `See:
below' answer NIL rather than being read as a tag called `Ship it'. Answers NIL
for a headline with none, which is most of them.

The word is delimited by `%org-blank-p' rather than by a space, which is org's
own `[ \t]' and one character more besides: a file that arrived from Windows has
a `#\Return' on the end of every line, and trimming only spaces would leave the
tag run ending in one and so read every tagged headline in the file as untagged."
  (let* ((trimmed (string-right-trim '(#\Space #\Tab #\Return) text))
         (space (position-if #'%org-blank-p trimmed :from-end t))
         (run (subseq trimmed (if space (1+ space) 0))))
    (when (and (> (length run) 2)
               (char= (char run 0) #\:)
               (char= (char run (1- (length run))) #\:))
      (remove "" (split-string run #\:) :test #'string=))))

(defun %org-agenda-scan (path text keep)
  "Rows for every headline in TEXT that KEEP accepts, as `PATH:LINE:TEXT'.

KEEP is called with (KEYWORD TAGS HEADING) and answers whether to list it, which
is the whole of what the two commands below differ by.

TAGS is what org means by a headline's tags rather than what is written on it: a
tag on `* Work' is carried by every task underneath it, and `#+FILETAGS:' by
every task in the file. Without that rule `org-agenda-tags' asked for `:work:'
answers with the one line that literally spells the word — which is a *heading*
and not a task, so the one row of the file that is no use at all, while every
task it was standing over is missing. The headline's text is untouched, so a row
still reads exactly as the file does.

STACK is one (LEVEL . TAGS) per open ancestor, deepest first. A headline closes
every entry at its own level or deeper — `member-if' finds the first shallower
one, and everything before it is a section this line has just ended — and then
opens its own. That is the whole of the inheritance rule; the file's own tags
are pushed at level 0, which no headline can ever close."
  (let ((rows nil) (n 0) (stack nil))
    (dolist (line (split-string text #\Newline) (nreverse rows))
      (incf n)
      (let ((parsed (%org-agenda-heading line)))
        (cond
          (parsed
           (destructuring-bind (stars keyword heading) parsed
             (setf stack (cons (cons stars (%org-agenda-tags heading))
                               (member-if (lambda (open) (< (car open) stars))
                                          stack)))
             (when (funcall keep keyword
                            (loop for open in stack append (cdr open))
                            heading)
               ;; The stars go back on, so the listing reads as an outline rather
               ;; than a flat list — depth is most of what a headline means.
               (push (format nil "~a:~a:~a ~a" path n
                             (make-string stars :initial-element #\*)
                             (string-trim '(#\Space #\Tab #\Return) heading))
                     rows))))
          ;; Org allows several of these and unions them, which falls out of
          ;; pushing rather than replacing. A headline above one does not get
          ;; them, which is org's rule too and the reason this is not read in a
          ;; pass of its own.
          ((%org-directive-p line "#+FILETAGS:")
           (push (cons 0 (%org-agenda-tags line)) stack)))))))

(defun %org-agenda-priority (row)
  "The letter in ROW's `[#A]' cookie, or `B' when it carries none.

Org's own default, and the surprising half of it is the default: an item with no
cookie sorts as though it had the middle priority, so `[#A]' floats above the
undecided majority and `[#C]' sinks below it. Read off the row rather than the
headline because the row is what there is by the time the files have been put
end to end — from *after* the `PATH:LINE:' prefix, so a directory with a `[#'
in its name cannot be read as a priority."
  (let* ((a (position #\: row))
         (b (and a (position #\: row :start (1+ a))))
         (at (and b (search "[#" row :start2 b))))
    (if (and at (< (+ at 3) (length row)) (char= (char row (+ at 3)) #\]))
        (char-upcase (char row (+ at 2)))
        #\B)))

(defun %org-agenda-text (path)
  "PATH's text — from the live buffer when that is the file, else from disk.

The buffer first, and it matters: the whole point of running this is to see the
thing you just wrote down, and a file you have not saved would otherwise be
listed as it was ten minutes ago.

Only the *live* buffer, though, and not `with-current-buffer': that matches by
buffer *name* rather than by path, and it genuinely switches the editor to the
other buffer and back — two switches per agenda file, each one a redraw and a
`buffer-switch-hook'. ponytail: so an unsaved file that is open in a pane you
are not standing in is read from disk and listed as it was last saved. The
upgrade is a reader that answers another buffer's text by path, which is the
same missing reader `with-current-buffer' names in its own ceiling."
  (if (equal path (buffer-file-name))
      (buffer-string)
      ;; NIL for a path that is not there, is a directory, or cannot be opened —
      ;; and `%org-agenda-show' names it rather than dropping it, so the three
      ;; are one answer here on purpose.
      (ignore-errors
        (with-open-file (in path :direction :input :external-format :utf-8)
          (let ((s (make-string (file-length in))))
            (subseq s 0 (read-sequence s in)))))))

(defun %org-agenda-show (keep title)
  "Scan the agenda files with KEEP and put the result up under TITLE.

`%expand-home' on every path, and it matters twice over: `with-open-file' would
read a literal `~' directory that is not there, and the row carries the path on
to `find-file-at', which joins a relative one onto the project root and would
land on `ROOT/~/org/work.org'. Expanding here rather than in `%org-agenda-text'
is what keeps those two spellings the same one — and is also what lets the live
buffer be recognised, since `buffer-file-name' has no `~' in it either.

A file that could not be read used to be dropped in silence, so a typo in
`*org-agenda-files*' reported `org: nothing unfinished' — which is a sentence
about your week rather than about your config, and the wrong one. It is named
now, and named *instead of* the empty answer only when there was no answer:
three files that scanned and one that did not still want the three."
  (let* ((named (if (stringp *org-agenda-files*)
                    ;; One path, unwrapped, is the config typo everybody makes
                    ;; and `loop for x in' answers it with a type error.
                    (list *org-agenda-files*)
                    *org-agenda-files*))
         (files (mapcar #'%expand-home
                        (or named
                            (let ((here (buffer-file-name)))
                              (and here (list here))))))
         (unread nil)
         (rows (stable-sort
                (loop for path in files
                      for text = (%org-agenda-text path)
                      if text append (%org-agenda-scan path text keep)
                      else do (push path unread))
                #'char< :key #'%org-agenda-priority)))
    (cond ((null files)
           (message "org: no agenda files — set *org-agenda-files*, or open one"))
          ;; STABLE-sort, so everything of equal priority is still in the order
          ;; it was written — file order across the list, outline order within a
          ;; file — which is the only other order these rows have that means
          ;; anything.
          (rows (xref-show (format nil "~a ~a~@[ (~a unreadable)~]"
                                   (length rows) title
                                   (and unread (length unread)))
                           rows))
          (unread (message (format nil "org: cannot read ~{~a~^, ~}"
                                   (nreverse unread))))
          (t (message (format nil "org: nothing ~a" title)))))
  nil)

(defun org-todo-list ()
  "Every unfinished item in the agenda files. Bound to `SPC m g'.

Unfinished means *carries a keyword that is not a done one* — a headline with no
keyword at all is a heading, not a task, and listing every headline in a file
would be an outline rather than an agenda."
  (%org-agenda-show
   (lambda (keyword tags heading)
     (declare (ignore tags heading))
     (and keyword (not (member keyword *org-agenda-done* :test #'string=))))
   "unfinished"))

(defun org-agenda-tags ()
  "Every item carrying a tag you name. Bound to `SPC m G'.

Asked rather than taken from point, because the question this answers is `what
is left under :work:' and the answer is usually about a tag you are not
currently sitting on. Inherited tags count — a task under `* Work :work:' is
under `:work:' whether or not it repeats the word, which is org's rule and the
way anybody who tags a file actually tags it."
  (read-string "Tag: "
    (lambda (tag)
      (when (and tag (plusp (length tag)))
        (let ((tag (string-trim ": " tag)))
          (%org-agenda-show
           (lambda (keyword tags heading)
             (declare (ignore keyword heading))
             (member tag tags :test #'string-equal))
           (format nil "tagged :~a:" tag))))))
  nil)

(export '(org-todo-list org-agenda-tags *org-agenda-files* *org-agenda-done*)
        :zemacs)
