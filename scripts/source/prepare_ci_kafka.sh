#!/usr/bin/env bash

# Exits as soon as any line fails.
set -e

SCRIPT_PATH="$(cd "$(dirname "$0")" >/dev/null 2>&1 && pwd)"
cd "$SCRIPT_PATH/.." || exit 1

echo "$SCRIPT_PATH"

if [ "$1" == "compress" ]; then
  echo "Compress test_data/ into test_data.zip"
  cd ./source
  zip_file=test_data.zip
  if [ -f "$zip_file" ]; then
    rm "$zip_file"
  fi
  zip -r "$zip_file" ./test_data/ch_benchmark/*
  exit 0
fi

echo "--- Extract data for Kafka"
cd ./source/
mkdir -p ./test_data/ch_benchmark/
unzip -o test_data.zip -d .
cd ..

echo "path:${SCRIPT_PATH}/test_data/**/*"

echo "Create topics"
kafka_data_files=$(find "$SCRIPT_PATH"/test_data -type f)
for filename in $kafka_data_files; do
    ([ -e "$filename" ]
    base=$(basename "$filename")
    topic="${base%%.*}"
    partition="${base##*.}"

    # always ok
    echo "Drop topic $topic"
    risedev rpk topic delete "$topic" || true

    echo "Recreate topic $topic with partition $partition"
    risedev rpk topic create "$topic" --partitions "$partition") &
done
wait

echo "Fulfill kafka topics"
python3 -m pip install --break-system-packages requests fastavro confluent_kafka jsonschema
for filename in $kafka_data_files; do
    ([ -e "$filename" ]
    base=$(basename "$filename")
    topic="${base%%.*}"

    echo "Fulfill kafka topic $topic with data from $base"
    # binary data, one message a file, filename/topic ends with "bin"
    if [[ "$topic" = *bin ]]; then
        kcat -P -b message_queue:29092 -t "$topic" "$filename"
    elif [[ "$topic" = *avro_json ]]; then
        python3 source/schema_registry_producer.py "message_queue:29092" "http://message_queue:8081" "$filename" "topic" "avro"
    elif [[ "$topic" = *json_schema ]]; then
        python3 source/schema_registry_producer.py "kafka:9093" "http://schemaregistry:8082" "$filename" "topic" "json"
    else
        cat "$filename" | kcat -P -K ^  -b message_queue:29092 -t "$topic"
    fi
    ) &
done

# test additional columns: produce messages with headers
ADDI_COLUMN_TOPIC="kafka_additional_columns"
for i in {0..100}; do echo "key$i:{\"a\": $i}" | kcat -P -b message_queue:29092 -t ${ADDI_COLUMN_TOPIC} -K : -H "header1=v1" -H "header2=v2"; done

# write schema with name strategy

## topic: upsert_avro_json-record, key subject: string, value subject: CPLM.OBJ_ATTRIBUTE_VALUE
(python3 source/schema_registry_producer.py  "message_queue:29092" "http://message_queue:8081" source/test_data/upsert_avro_json.1 "record" "avro") &
## topic: upsert_avro_json-topic-record,
## key subject: upsert_avro_json-topic-record-string
## value subject: upsert_avro_json-topic-record-CPLM.OBJ_ATTRIBUTE_VALUE
(python3 source/schema_registry_producer.py  "message_queue:29092" "http://message_queue:8081" source/test_data/upsert_avro_json.1 "topic-record" "avro") &
wait
