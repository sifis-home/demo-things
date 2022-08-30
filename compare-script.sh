#!/bin/bash

# This script computes a comparison among two different versions of the same
# demo-things repository. Choose the demo name you want to compare as input.
#
# Usage: ./compare-script.sh DEMO_NAME
#
# Example: ./compare-script.sh lamp

# Stops at the first error
set -e

# Check whether a demo name has been passed as input
if [ -z "$1" ]
then
    echo "No demo name has been passed as input"
    exit 1
fi

# Device name
DEMO=$1

# Time filename
TIME="time.txt"
# Filesize filename
FILESIZE="filesize.txt"
# Snippets directory
SNIPPETS="snippets"
# Metrics directory
METRICS="metrics"

# Comparison directory name
COMPARE_DIR="Compare"

# Obtains metrics
function obtain-metrics () {
    # Compare directory path
    COMPARE_PATH=$COMPARE_DIR/$1

    # Checkout commit
    git checkout $1

    # Create version commit directory
    mkdir -p $COMPARE_PATH

    # Get building time through `/usr/bin/time` tool
    /usr/bin/time -o $COMPARE_PATH/$TIME cargo build --release --bin $DEMO

    # Get binary size through `du` tool
    du -h -s target/release/$DEMO > $COMPARE_PATH/$FILESIZE

    # Remove binaries
    cargo clean

    # Get complexity snippets (only cyclomatic for now)
    complex-code-spotter -O json src/ $COMPARE_PATH/$SNIPPETS

    # Create static analysis directory
    mkdir -p $COMPARE_PATH/$METRICS

    # Get static analysis metrics
    rust-code-analysis-cli --metrics -O json --pr -o "$COMPARE_PATH/$METRICS" -p src/
}

function write-header () {
echo -e '| hash | size | time | sloc | halstead (difficulty) | cyclomatic | cognitive |' >> report.md
    echo -e '|  -|  -|  -|  -|  -|  -|  -|' >> report.md
}

# Compute time and filesize differences
function write-report () {
    COMPARE=$COMPARE_DIR/$1

    t=`sed -e "s:[[:space:]]*\([^ ]*\).*:\1:" $COMPARE/$TIME`
    h=`git describe --all $1`
    s=`cut -f 1 $COMPARE/$FILESIZE`
    metrics=$COMPARE/metrics/src_bin_${DEMO}.rs.json

    sl=`jq '.metrics | .loc.sloc' $metrics`
    hd=`jq '.metrics | .halstead.difficulty  * 100 | round / 100' $metrics`
    cy=`jq '.metrics | .cyclomatic.average   * 100 | round / 100' $metrics`
    co=`jq '.metrics | .cognitive.average    * 100 | round / 100' $metrics`

    echo -e "| ${h} | ${s} | ${t} | ${sl} | ${hd} | ${cy} | ${co} |" >> report.md
}

COMMITS="webthings wot-td"
# Obtain metrics
#for commit in $COMMITS; do
#    obtain-metrics $commit
#done

# make a report with the binary size, build time, complexity

write-header
for commit in $COMMITS; do
    write-report $commit
done
