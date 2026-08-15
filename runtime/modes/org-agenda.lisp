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
can see it.")

(defparameter *org-agenda-done* '("DONE" "CANCELLED")
  "Keywords that mean *finished*, and are therefore left out of the list.

Separate from `*org-todo-keywords*' — which is the cycle, in order — because
which of those states counts as done is a different question from which exist. A
config adding `WAIT' to the cycle wants it in the agenda; one adding `CANCELLED'
does not.")

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
for a headline with none, which is most of them."
  (let* ((trimmed (string-right-trim " " text))
         (space (position #\Space trimmed :from-end t))
         (run (subseq trimmed (if space (1+ space) 0))))
    (when (and (> (length run) 2)
               (char= (char run 0) #\:)
               (char= (char run (1- (length run))) #\:))
      (remove "" (split-string run #\:) :test #'string=))))

(defun %org-agenda-scan (path text keep)
  "Rows for every headline in TEXT that KEEP accepts, as `PATH:LINE:TEXT'.

KEEP is called with (KEYWORD TAGS HEADING) and answers whether to list it, which
is the whole of what the two commands below differ by."
  (let ((rows nil) (n 0))
    (dolist (line (split-string text #\Newline) (nreverse rows))
      (incf n)
      (let ((parsed (%org-agenda-heading line)))
        (when parsed
          (destructuring-bind (stars keyword heading) parsed
            (when (funcall keep keyword (%org-agenda-tags heading) heading)
              ;; The stars go back on, so the listing reads as an outline rather
              ;; than a flat list — depth is most of what a headline means.
              (push (format nil "~a:~a:~a ~a" path n
                            (make-string stars :initial-element #\*)
                            (string-trim " " heading))
                    rows))))))))

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
      (ignore-errors
        (with-open-file (in path :direction :input :external-format :utf-8)
          (let ((s (make-string (file-length in))))
            (subseq s 0 (read-sequence s in)))))))

(defun %org-agenda-show (keep title)
  "Scan the agenda files with KEEP and put the result up under TITLE."
  (let* ((files (or *org-agenda-files*
                    (let ((here (buffer-file-name)))
                      (and here (list here)))))
         (rows (loop for path in files
                     for text = (%org-agenda-text path)
                     when text append (%org-agenda-scan path text keep))))
    (cond ((null files)
           (message "org: no agenda files — set *org-agenda-files*, or open one"))
          ((null rows) (message (format nil "org: nothing ~a" title)))
          (t (xref-show (format nil "~a ~a" (length rows) title) rows))))
  nil)

(defun org-todo-list ()
  "Every unfinished item in the agenda files. Bound to `SPC m a'.

Unfinished means *carries a keyword that is not a done one* — a headline with no
keyword at all is a heading, not a task, and listing every headline in a file
would be an outline rather than an agenda."
  (%org-agenda-show
   (lambda (keyword tags heading)
     (declare (ignore tags heading))
     (and keyword (not (member keyword *org-agenda-done* :test #'string=))))
   "unfinished"))

(defun org-agenda-tags ()
  "Every item carrying a tag you name. Bound to `SPC m A'.

Asked rather than taken from point, because the question this answers is `what
is left under :work:' and the answer is usually about a tag you are not
currently sitting on."
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
