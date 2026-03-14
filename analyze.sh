mkdir -p ./analysis  \
&& cargo build --profile profiling \
&& cp ./config.toml ./target/profiling/ \
&& cd ./target/profiling/ \
&& samply record ./serekes