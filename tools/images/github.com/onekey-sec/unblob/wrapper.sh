#!/bin/sh

# needed to get pip install of unblob as unblob user to work
export HOME="/home/unblob"
# run unblob
/usr/local/bin/unblob --report /tmp/thorium/results -e /tmp/thorium/children/unpacked/ --log /tmp/thorium/result-files/unblob.log $1
# get the name of the sample we ran on
filename=$(basename "$1")
# move the contents of our extract path up a dir to remove the extra _extract dir
# using this approach to include hidden files that the mv command would miss
find "/tmp/thorium/children/unpacked/${filename}_extract/" -mindepth 1 -maxdepth 1 -exec mv -t "/tmp/thorium/children/unpacked/." -- {} +
# remove the empty extract dir
rm -rf /tmp/thorium/children/unpacked/${filename}_extract
