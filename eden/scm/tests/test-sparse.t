  $ setconfig extensions.treemanifest=!
test sparse

  $ enable sparse
  $ hg init myrepo
  $ cd myrepo

  $ echo a > show
  $ echo x > hide
  $ hg ci -Aqm 'initial'

  $ echo b > show
  $ echo y > hide
  $ echo aa > show2
  $ echo xx > hide2
  $ hg ci -Aqm 'two'

Verify basic include

  $ hg up -q 0
  $ hg sparse include 'hide'
  $ ls
  hide

Absolute paths outside the repo should just be rejected

  $ hg sparse include /foo/bar
  abort: paths cannot be absolute
  [255]
  $ hg sparse include '$TESTTMP/myrepo/hide'

  $ hg sparse include '/root'
  abort: paths cannot be absolute
  [255]

Repo root-relaive vs. cwd-relative includes
  $ mkdir subdir
  $ cd subdir
  $ hg sparse include --config sparse.includereporootpaths=on notinsubdir/path
  $ hg sparse include --config sparse.includereporootpaths=off **/path
  $ hg sparse include --config sparse.includereporootpaths=off path:abspath
  $ hg sparse
  [include]
  $TESTTMP/myrepo/hide
  hide
  notinsubdir/path
  path:abspath
  subdir/**/path
  [exclude]
  
  
  $ cd ..
  $ rm -rf subdir

Verify deleting uses relative paths
  $ mkdir subdir && echo foo > subdir/foo
  $ hg sparse
  [include]
  $TESTTMP/myrepo/hide
  hide
  notinsubdir/path
  path:abspath
  subdir/**/path
  [exclude]
  
  
  $ cd subdir
  $ hg sparse --delete **/path
  $ hg sparse
  [include]
  $TESTTMP/myrepo/hide
  hide
  notinsubdir/path
  path:abspath
  [exclude]
  
  
  $ cd ..
  $ rm -rf subdir

Verify commiting while sparse includes other files

  $ echo z > hide
  $ hg ci -Aqm 'edit hide'
  $ ls
  hide
  $ hg manifest
  hide
  show
  $ hg files
  hide

Verify --reset brings files back

  $ hg sparse --reset
  $ ls
  hide
  show
  $ cat hide
  z
  $ cat show
  a

Verify 'hg sparse' default output

  $ hg up -q null
  $ hg sparse include 'show*'

  $ hg sparse
  [include]
  show*
  [exclude]
  
  
Verify update only writes included files

  $ hg up -q 0
  $ ls
  show

  $ hg up -q 1
  $ ls
  show
  show2

Verify status only shows included files

  $ touch hide
  $ touch hide3
  $ echo c > show
  $ hg status
  M show

Adding an excluded file should fail

  $ hg add hide3
  abort: cannot add 'hide3' - it is outside the sparse checkout
  (include file with `hg sparse include <pattern>` or use `hg add -s <file>` to include file directory while adding)
  [255]

Verify deleting sparseness while a file has changes fails

  $ hg sparse uninclude 'show*'
  pending changes to 'hide'
  abort: cannot change sparseness due to pending changes (delete the files or use --force to bring them back dirty)
  [255]

Verify deleting sparseness with --force brings back files

  $ hg sparse uninclude -f 'show*'
  pending changes to 'hide'
  $ ls
  hide
  hide2
  hide3
  show
  show2
  $ hg st
  M hide
  M show
  ? hide3

Verify editing sparseness fails if pending changes

  $ hg sparse include 'show*'
  pending changes to 'hide'
  abort: could not update sparseness due to pending changes
  [255]

Verify adding sparseness hides files

  $ hg sparse exclude -f 'hide*'
  pending changes to 'hide'
  $ ls
  hide
  hide3
  show
  show2
  $ hg st
  M show

  $ hg up -qC .
  $ hg purge --all --config extensions.purge=
  $ ls
  show
  show2

Verify rebase temporarily includes excluded files

  $ hg rebase -d 1 -r 2 --config extensions.rebase=
  rebasing b91df4f39e75 "edit hide" (tip)
  temporarily included 1 file(s) in the sparse checkout for merging
  merging hide
  warning: 1 conflicts while merging hide! (edit, then use 'hg resolve --mark')
  unresolved conflicts (see hg resolve, then hg rebase --continue)
  [1]

  $ hg sparse
  [include]
  
  [exclude]
  hide*
  
  Temporarily Included Files (for merge/rebase):
  hide

  $ cat hide
  <<<<<<< dest:   39278f7c08a9 - test: two
  y
  =======
  z
  >>>>>>> source: b91df4f39e75 - test: edit hide

Verify aborting a rebase cleans up temporary files

  $ hg rebase --abort --config extensions.rebase=
  cleaned up 1 temporarily added file(s) from the sparse checkout
  rebase aborted
  $ rm hide.orig

  $ ls
  show
  show2

Verify merge fails if merging excluded files

  $ hg up -q 1
  $ hg merge -r 2
  temporarily included 1 file(s) in the sparse checkout for merging
  merging hide
  warning: 1 conflicts while merging hide! (edit, then use 'hg resolve --mark')
  0 files updated, 0 files merged, 0 files removed, 1 files unresolved
  use 'hg resolve' to retry unresolved file merges or 'hg update -C .' to abandon
  [1]
  $ hg sparse
  [include]
  
  [exclude]
  hide*
  
  Temporarily Included Files (for merge/rebase):
  hide

  $ hg up -C .
  cleaned up 1 temporarily added file(s) from the sparse checkout
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved
  $ hg sparse
  [include]
  
  [exclude]
  hide*
  

Verify strip -k resets dirstate correctly

  $ hg status
  $ hg sparse
  [include]
  
  [exclude]
  hide*
  
  $ hg log -r . -T '{rev}\n' --stat
  1
   hide  |  2 +-
   hide2 |  1 +
   show  |  2 +-
   show2 |  1 +
   4 files changed, 4 insertions(+), 2 deletions(-)
  
  $ hg debugstrip -r . -k
  saved backup bundle to $TESTTMP/myrepo/.hg/strip-backup/39278f7c08a9-ce59e002-backup.hg (glob)
  $ hg status
  M show
  ? show2

Verify rebase succeeds if all changed files are in sparse checkout

  $ hg commit -Aqm "add show2"
  $ hg rebase -d 1 --config extensions.rebase=
  rebasing bdde55290160 "add show2" (tip)

Verify log --sparse only shows commits that affect the sparse checkout

  $ hg log -T '{rev} '
  3 1 0  (no-eol)
  $ hg log --sparse -T '{rev} '
  3 0  (no-eol)

Test status on a file in a subdir

  $ mkdir -p dir1/dir2
  $ touch dir1/dir2/file
  $ hg sparse -I dir1/dir2
  $ hg status
  ? dir1/dir2/file

Test that add -s adds dirs to sparse profile

  $ hg sparse --reset
  $ hg sparse include empty
  $ hg sparse
  [include]
  empty
  [exclude]
  
  

  $ mkdir add
  $ touch add/foo
  $ touch add/bar
  $ hg add add/foo
  abort: cannot add 'add/foo' - it is outside the sparse checkout
  (include file with `hg sparse include <pattern>` or use `hg add -s <file>` to include file directory while adding)
  [255]
  $ hg add -s add/foo
  $ hg st
  A add/foo
  ? add/bar
  $ hg sparse
  [include]
  add
  empty
  [exclude]
  
  
  $ hg add -s add/*
  add/foo already tracked!
  $ hg st
  A add/bar
  A add/foo
  $ hg sparse
  [include]
  add
  empty
  [exclude]
  
  
Test --cwd-list
  $ hg commit -m 'commit'
  $ hg sparse --cwd-list
    add
  - hide
  - show
  - show2
  $ cd add
  $ hg sparse --cwd-list
    bar
    foo
  $ hg sparse include foo
  $ hg sparse uninclude .
  $ hg sparse show
  Additional Included Paths:
  
    add/foo
    empty
  $ hg sparse --cwd-list
  - bar
    foo

Make sure to match whole directory names, not prefixes

  $ mkdir prefix prefixpostfix
  $ touch prefix/correct prefixpostfix/incorrect
  $ hg sparse -I prefix prefixpostfix
  $ hg addremove .
  adding prefix/correct
  adding prefixpostfix/incorrect
  $ hg ci -m 'subdirs'
  $ cd prefix
  $ hg sparse --cwd-list
    correct
  $ cd ../..

  $ cd ..

Test non-sparse repos work while sparse is loaded
  $ hg init sparserepo
  $ hg init nonsparserepo
  $ cd sparserepo
  $ cat > .hg/hgrc <<EOF
  > [extensions]
  > sparse=
  > EOF
  $ cd ../nonsparserepo
  $ echo x > x && hg add x && hg commit -qAm x
  $ cd ../sparserepo
  $ hg clone ../nonsparserepo ../nonsparserepo2
  updating to branch default
  1 files updated, 0 files merged, 0 files removed, 0 files unresolved

Test debugrebuilddirstate
  $ cd ../sparserepo
  $ touch included
  $ touch excluded
  $ hg add included excluded
  $ hg commit -m 'a commit' -q
  $ cp .hg/dirstate ../dirstateboth
  $ hg sparse -X excluded
  $ cp ../dirstateboth .hg/dirstate
  $ hg debugrebuilddirstate
  $ hg debugdirstate
  n   0         -1 unset               included

Test debugdirstate --minimal where file is in the parent manifest but not the
dirstate
  $ hg sparse -X included
  $ hg debugdirstate
  $ cp .hg/dirstate ../dirstateallexcluded
  $ hg sparse --reset
  $ hg sparse -X excluded
  $ cp ../dirstateallexcluded .hg/dirstate
  $ touch includedadded
  $ hg add includedadded
  $ hg debugdirstate --nodates
  a   0         -1 unset               includedadded
  $ hg debugrebuilddirstate --minimal
  $ hg debugdirstate --nodates
  n   0         -1 unset               included
  a   0         -1 * includedadded (glob)

Test debugdirstate --minimal where a file is not in parent manifest
but in the dirstate. This should take into account excluded files in the
manifest
  $ cp ../dirstateboth .hg/dirstate
  $ touch includedadded
  $ hg add includedadded
  $ touch excludednomanifest
  $ hg add excludednomanifest
  $ cp .hg/dirstate ../moreexcluded
  $ hg forget excludednomanifest
  $ rm excludednomanifest
  $ hg sparse -X excludednomanifest
  $ cp ../moreexcluded .hg/dirstate
  $ hg manifest
  excluded
  included
We have files in the dirstate that are included and excluded. Some are in the
manifest and some are not.
  $ hg debugdirstate --nodates
  n 644          0 * excluded (glob)
  a   0         -1 * excludednomanifest (glob)
  n 644          0 * included (glob)
  a   0         -1 * includedadded (glob)
  $ hg debugrebuilddirstate --minimal
  $ hg debugdirstate --nodates
  n 644          0 * included (glob)
  a   0         -1 * includedadded (glob)

Test logging the dirsize and sparse profiles

Set up the sampling extension and set a log file, then do a repo status.
We need to disable the SCM_SAMPLING_FILEPATH env var because arcanist may set it!

  $ touch a && hg add a
  $ unset SCM_SAMPLING_FILEPATH
  $ hg ci -m "add some new files"
  $ LOGDIR=`pwd`/logs
  $ mkdir $LOGDIR
  $ cat >> $HGRCPATH << EOF
  > [sampling]
  > key.dirstate_size=dirstate_size
  > key.sparse_profiles=sparse_profiles
  > filepath = $LOGDIR/samplingpath.txt
  > [extensions]
  > sampling=
  > EOF
  $ rm -f $LOGDIR/samplingpath.txt
  $ hg status
  >>> import json
  >>> with open("$LOGDIR/samplingpath.txt") as f:
  ...     data = f.read()
  >>> for record in data.strip("\0").split("\0"):
  ...     parsedrecord = json.loads(record)
  ...     if parsedrecord['category'] == 'dirstate_size':
  ...         print('{0}: {1}'.format(parsedrecord['category'],
  ...                                 parsedrecord['data']['dirstate_size']))
  dirstate_size: 3
  $ cat >> profile_base << EOF
  > [include]
  > a
  > EOF
  $ cat >> profile_extended << EOF
  > %include profile_base
  > EOF
  $ cat >> $HGRCPATH << EOF
  > [sparse]
  > largecheckouthint=True
  > largecheckoutcount=1
  > EOF
  $ hg add profile_base profile_extended
  hint[sparse-largecheckout]: Your repository checkout has * files which makes Many mercurial commands slower. Learn how to make it smaller at https://fburl.com/hgsparse (glob)
  hint[hint-ack]: use 'hg hint --ack sparse-largecheckout' to silence these hints
  $ hg ci -m 'adding sparse profiles'
  hint[sparse-largecheckout]: Your repository checkout has * files which makes Many mercurial commands slower. Learn how to make it smaller at https://fburl.com/hgsparse (glob)
  hint[hint-ack]: use 'hg hint --ack sparse-largecheckout' to silence these hints
  $ rm -f $LOGDIR/samplingpath.txt
  $ hg sparse --enable-profile profile_extended
  hint[sparse-largecheckout]: Your repository checkout has * files which makes Many mercurial commands slower. Learn how to make it smaller at https://fburl.com/hgsparse (glob)
  hint[hint-ack]: use 'hg hint --ack sparse-largecheckout' to silence these hints
  >>> import json
  >>> with open("$LOGDIR/samplingpath.txt") as f:
  ...     data = f.read()
  >>> for record in data.strip("\0").split("\0"):
  ...     parsedrecord = json.loads(record)
  ...     if parsedrecord['category'] == 'sparse_profiles':
  ...         print('active_profiles:', parsedrecord['data']['active_profiles'])
  active_profiles: profile_base,profile_extended