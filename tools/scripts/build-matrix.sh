#!/bin/bash
PIPELINES=$(toml get manifest.toml . | jq '.pipelines | keys[]' | tr -d '\"')
MATRIX_ARRAY=()
for pipeline in $PIPELINES; do
  # Get a list of images for that pipeline
  IMAGES=$(toml get manifest.toml . | jq ".pipelines.\"${pipeline}\" | keys[]"  | tr -d '\"' | grep -v -e url)
  for image in $IMAGES; do
    IMAGE_PATH=$(toml get manifest.toml . | jq ".pipelines.\"${pipeline}\".\"${image}\".build_path")
    # images must be lower case so there could be collisions bacause of the  `tr '[A-Z]' '[a-z]'`
    IMAGE_NAME=$(toml get manifest.toml . | jq ".pipelines.\"${pipeline}\".\"${image}\".image_name" | tr '[A-Z]' '[a-z]')
    MATRIX_ARRAY+=("{\"path\": $IMAGE_PATH, \"name\": $IMAGE_NAME}")
  done
done
# build raw JSON array
MATRIX_JSON=$(printf ",%s" "${MATRIX_ARRAY[@]}")
# Remove leading comma
MATRIX_JSON="[${MATRIX_JSON:1}]"
echo $MATRIX_JSON
