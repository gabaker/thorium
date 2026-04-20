#!/bin/sh

FNAME=$(basename $1)

echo "Unpacking $FNAME"

upx -o /tmp/thorium/children/unpacked/$FNAME -d $1
cd /tmp/thorium/children/unpacked && mv $FNAME $(sha256sum $FNAME | awk '{print $1}')