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

# First commit sha
FIRST_COMMIT="ccd8fff"
# Second commit sha
SECOND_COMMIT="master"

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

# Compute time and filesize differences
function time-filesize-difference () {
    # First compare path
    FIRST_COMPARE=$COMPARE_DIR/$FIRST_COMMIT

    # Second compare path
    SECOND_COMPARE=$COMPARE_DIR/$SECOND_COMMIT

    echo -e '| hash | size | time |' >> report.md
    echo -e '|  -|  -|  -|' >> report.md

    t=$FIRST_COMPARE/$TIME
    h=$FIRST_COMMIT
    s=`cut -f 1 $FIRST_COMPARE/$FILESIZE`
    echo -e "| ${h} | ${s} | ${t} |" >> report.md
    t=$SECOND_COMPARE/$TIME
    h=$SECOND_COMMIT
    s=`cut -f 1 $FIRST_COMPARE/$FILESIZE`
    echo -e "| ${h} | ${s} | ${t} |" >> report.md
}

# Compute differences among json files
function json-differences () {
    # Define path to the directory containing json differences
    COMPARE_DIFF=$COMPARE_DIR/$2

    # First compare path
    FIRST_COMPARE=$COMPARE_DIR/$FIRST_COMMIT

    # Second compare path
    SECOND_COMPARE=$COMPARE_DIR/$SECOND_COMMIT

    # Exit whether the first json directory does not exist
    if [ ! -d "$FIRST_COMPARE/$1" ]
    then
      echo "$FIRST_COMPARE/$1 does not exist"
      return 0
    fi

    # Exit whether the second json directory does not exist
    if [ ! -d "$SECOND_COMPARE/$1" ]
    then
      echo "$SECOND_COMPARE/$1 does not exist"
      return 0
    fi

    # All json paths from the two directories to compare
    # Remove common path prefixes and sort out the outcomes
    FIRST_JSON_FILES=`find $FIRST_COMPARE/$1 -type f -name "*.json" | cut -d'/' -f4- | LC_ALL=C sort`
    SECOND_JSON_FILES=`find $SECOND_COMPARE/$1 -type f -name "*.json" | cut -d'/' -f4- | LC_ALL=C sort`

    # Get intersection among the two arrays
    INTERSECT=`comm -12 <(printf '%s\n' "${FIRST_JSON_FILES[@]}") <(printf '%s\n' "${SECOND_JSON_FILES[@]}")`

    # Exit when the intersect array is empty
    if [ -z "$INTERSECT" ]
    then
        return 0
    fi

    # Create json differences directory
    mkdir -p $COMPARE_DIFF

    # Iter over the intersection array and make comparisons through `jq`
    for JSON_FILE in $INTERSECT
    do
        OUTPUT_FILE=`echo $JSON_FILE | tr '/' '_'`
        diff -urN <(jq -S . $FIRST_COMPARE/$1/$JSON_FILE ) <(jq -S . $SECOND_COMPARE/$1/$JSON_FILE ) > $COMPARE_DIFF/$OUTPUT_FILE || :
        # When a file is empty, remove it
        [ ! -s $COMPARE_DIFF/$OUTPUT_FILE ] && rm -f $COMPARE_DIFF/$OUTPUT_FILE
    done
}


# Obtain metrics for the first commit
obtain-metrics $FIRST_COMMIT

# Obtain metrics for the second commit
obtain-metrics $SECOND_COMMIT

# Compute time and filesize differences
time-filesize-difference

# Compute snippets json differences
json-differences $SNIPPETS "compare-diff"

# Compute rust-code-analysis json differences
json-differences $METRICS "rca-diff"
