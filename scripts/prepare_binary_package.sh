#!/bin/bash -e
# LICENSE BEGIN
# This file is part of the SixtyFPS Project -- https://sixtyfps.io
# Copyright (c) 2021 Olivier Goffart <olivier.goffart@sixtyfps.io>
# Copyright (c) 2021 Simon Hausmann <simon.hausmann@sixtyfps.io>
#
# SPDX-License-Identifier: GPL-3.0-only
# This file is also available under commercial licensing terms.
# Please contact info@sixtyfps.io for more information.
# LICENSE END

if [ $# != 3 ]; then
    echo "usage: $0 path/to/target/binary_package path/to/binary/Qt qt_version"
    echo
    echo "This prepares the specified binary_package folder for distribution"
    echo "by adding the legal copyright and license notices."
    echo
    echo "All files will be copied/created under the 3rdparty-licenses folder"
    echo "along with an index.html"
    echo
    echo "(The path to Qt could be for example ~/Qt/ where the qt installer placed"
    echo " the binaries and sources under)"
    exit 1
fi

target_path=$1/3rdparty-licenses
qt_path=$2
qt_version=$3

mkdir -p $target_path
cp -a `dirname $0`/../LICENSE.md $target_path

cp ~/.cargo/registry/src/github.com-1ecc6299db9ec823/sixtyfps-rendering-backend-qt-0.1.4/LICENSE.QT sixtyfps_runtime/rendering_backends/qt/QtThirdPartySoftware_Listing.txt $target_path/
