;;;; org-export — the org buffer you are in, as a LaTeX buffer beside it.
;;;;
;;;; A line-at-a-time translator in plain Common Lisp: ECL ships no regexp
;;;; engine, and none is needed for org's line-shaped grammar. `M-x
;;;; org-export-latex' reads `buffer-string', writes `*org-latex*' with
;;;; `create-buffer' + `replace-region', and lands you in it. Nothing is saved
;;;; unless you `save-file' it somewhere.
;;;;
;;;; ponytail: headlines, TODO keywords, one level of - / + / N. lists,
;;;; #+begin_src / quote / example blocks, #+TITLE, *bold* /italic/ =code= ~code~
;;;; and &%# escaping. Not: nested lists, tables, links, footnotes, drawers
;;;; (`:PROPERTIES:' lines pass through as text). Add a rung when a file of
;;;; yours needs it.

(in-package :zemacs)

(defun %org-latex-escape (s)
  "Escape the three characters org prose uses and LaTeX will not take.
`$', `_' and `^' are left alone on purpose: org notes with math in them want
them raw, and `\\$' in a formula is worse than `&' in prose."
  (with-output-to-string (out)
    (loop for c across s
          do (case c
               ((#\& #\% #\#) (write-char #\\ out) (write-char c out))
               (t (write-char c out))))))

(defun %org-latex-inline (s)
  "`*b*' -> \\textbf{b}, `/i/' -> \\emph{i}, `=c=' and `~c~' -> \\texttt{c}.
A marker opens only at line start or after whitespace/punctuation, followed by
a non-space, and closes at the next same marker that is followed by
end/space/punctuation — org's own rule, minus the multi-line span."
  (let ((n (length s)) (i 0) (out (make-string-output-stream)))
    (flet ((wrap (c) (case c (#\* "textbf") (#\/ "emph") ((#\= #\~) "texttt")))
           (boundary-p (c) (or (null c) (member c '(#\Space #\Tab #\( #\) #\, #\. #\; #\: #\! #\? #\- #\")))))
      (loop while (< i n)
            do (let* ((c (char s i))
                      (cmd (wrap c))
                      (prev (and (plusp i) (char s (1- i))))
                      (close (and cmd
                                  (boundary-p prev)
                                  (< (1+ i) n)
                                  (not (member (char s (1+ i)) '(#\Space #\Tab)))
                                  (loop for j from (+ i 2) below n
                                        when (and (char= (char s j) c)
                                                  (not (member (char s (1- j)) '(#\Space #\Tab)))
                                                  (boundary-p (and (< (1+ j) n) (char s (1+ j)))))
                                          return j))))
                 (cond (close
                        (format out "\\~a{~a}" cmd (%org-latex-escape (subseq s (1+ i) close)))
                        (setf i (1+ close)))
                       (t (write-string (%org-latex-escape (string c)) out)
                          (incf i))))))
    (get-output-stream-string out)))

(defun %org-latex-headline (line)
  "`** TODO Title :tag:' -> (2 . \"Title\"), or NIL for a non-headline."
  (let ((stars (position-if-not (lambda (c) (char= c #\*)) line)))
    (when (and stars (plusp stars) (< stars (length line)) (char= (char line stars) #\Space))
      (let* ((title (string-trim " " (subseq line stars)))
             (words (split-string title #\Space)))
        (when (member (first words) *org-todo-keywords* :test #'string=)
          (setf title (string-trim " " (subseq title (length (first words))))))
        ;; Trailing `:a:b:' tags, dropped.
        (let ((sp (position #\Space title :from-end t)))
          (when (and sp (> (length title) (+ sp 2))
                     (char= (char title (1+ sp)) #\:)
                     (char= (char title (1- (length title))) #\:))
            (setf title (string-trim " " (subseq title 0 sp)))))
        (cons stars title)))))

(defun %org-latex-list-item (line)
  "`- x' / `+ x' -> (:itemize . \"x\"); `3. x' / `3) x' -> (:enumerate . \"x\"); else NIL."
  (let* ((s (string-left-trim " " line))
         (sp (position #\Space s)))
    (cond ((and sp (= sp 1) (member (char s 0) '(#\- #\+)))
           (cons :itemize (subseq s 2)))
          ((and sp (> sp 1) (every #'digit-char-p (subseq s 0 (1- sp)))
                (member (char s (1- sp)) '(#\. #\))))
           (cons :enumerate (subseq s (1+ sp))))
          (t nil))))

(defun %org-latex-keyword (line)
  "`#+NAME: value' -> (\"name\" . \"value\"), lowercase name; else NIL."
  (when (and (> (length line) 2) (string= "#+" line :end2 2))
    (let ((colon (position #\: line)))
      (cons (string-downcase (subseq line 2 (or colon (position #\Space line) (length line))))
            (and colon (string-trim " " (subseq line (1+ colon))))))))

(defun org->latex (text)
  "TEXT, an org document, as a complete LaTeX document string."
  (let ((lines (split-string text #\Newline))
        (title nil) (author nil) (body '()) (list nil) (block nil))
    (flet ((emit (s) (push s body))
           (close-list ()
             (when list (push (format nil "\\end{~(~a~)}" list) body) (setf list nil))))
      (dolist (line lines)
        (let ((kw (%org-latex-keyword line)))
          (cond
            ;; Inside a block: src/example verbatim, the rest still marked up.
            (block
             (if (and kw (string= (car kw) (format nil "end_~a" block)))
                 (progn (emit (format nil "\\end{~a}"
                                      (cond ((string= block "src") "verbatim")
                                            ((string= block "example") "verbatim")
                                            (t block))))
                        (setf block nil))
                 (emit (if (member block '("src" "example") :test #'string=)
                           line
                           (%org-latex-inline line)))))
            ((and kw (string= (car kw) "title")) (setf title (cdr kw)))
            ((and kw (string= (car kw) "author")) (setf author (cdr kw)))
            ((and kw (> (length (car kw)) 6) (string= "begin_" (car kw) :end2 6))
             (close-list)
             (setf block (subseq (car kw) 6))
             (emit (format nil "\\begin{~a}"
                           (cond ((string= block "src") "verbatim")
                                 ((string= block "example") "verbatim")
                                 (t block)))))
            (kw nil)                    ; other #+ settings: dropped
            ((%org-latex-headline line)
             (close-list)
             (let ((h (%org-latex-headline line)))
               (emit (format nil "\\~a{~a}"
                             (case (car h) (1 "section") (2 "subsection") (3 "subsubsection") (t "paragraph"))
                             (%org-latex-inline (cdr h))))))
            ((%org-latex-list-item line)
             (let ((item (%org-latex-list-item line)))
               (unless (eq list (car item))
                 (close-list)
                 (setf list (car item))
                 (emit (format nil "\\begin{~(~a~)}" list)))
               (emit (format nil "  \\item ~a" (%org-latex-inline (cdr item))))))
            ((string= (string-trim " " line) "")
             (close-list)
             (emit ""))
            (t (emit (%org-latex-inline line))))))
      (close-list))
    (format nil "\\documentclass{article}~%\\usepackage[utf8]{inputenc}~%\\usepackage{amsmath}~%~
                 ~@[\\title{~a}~%~]~@[\\author{~a}~%~]~%\\begin{document}~%~:[~;\\maketitle~%~]~%~
                 ~{~a~%~}~%\\end{document}~%"
            (and title (%org-latex-inline title)) (and author (%org-latex-inline author))
            title (nreverse body))))

(defun org-export-latex ()
  "The org buffer you are in, translated into a new `*org-latex*' buffer.
Bound to `SPC m X'."
  (let ((latex (org->latex (buffer-string)))
        (from (buffer-name)))
    (create-buffer "*org-latex*")
    (replace-region 0 (point-max) latex)
    (goto-char 0)
    (message (format nil "org: ~a as LaTeX — save-file to keep it" from)))
  nil)

(export '(org-export-latex org->latex) :zemacs)
