# Golem

![Golem Logo](golem-logo-black.jpg)

This repository contains Golem - a set of services enable you to run WebAssembly components in a distributed cloud environment.

See [Golem Cloud](https://golem.cloud) for more information.

## Getting Started

It is possible to start using Golem locally by using our published Docker containers. For compiling it for yourself please check [the contribution guide](CONTRIBUTING.md).

Firstly, spin up golem services using docker-compose in [docker-examples](docker-examples) folder. Please note that there is .env file consisting 
of common port configurations. You can override these variables in .env file in your local if there are port conflicts. Consider these examples as a simple reference for you to spin up the OSS golem services quickly and try things out. You may have a different approach and a different practice on how to configure docker!

```
curl -O https://raw.githubusercontent.com/golemcloud/golem-services/main/docker-examples/docker-compose-sqlite.yaml -O https://raw.githubusercontent.com/golemcloud/golem-services/main/docker-examples/.env
docker-compose -f docker-compose-sqlite.yaml up

```
Afterwards in a separate terminal,

```bash

cargo install golem-cli

# template is your compiled code written in Rust, C, etc
# https://learn.golem.cloud/docs/building-templates helps you write some code and create a template - as an example
golem-cli template add <location-to-template-file> 

# Now we need a worker corresponding from template, that can execute one of the functions in template
# If worker doesn't exist, it is created on the fly whey you invoke a function in template
golem-cli worker invoke-and-await  --template-id <template-id> --worker-name my-worker --function golem:it/api/add-item --parameters '[{"product-id" : "foo", "name" : "foo" , "price" : 10, "quantity" : 1}]'

```

Internally, it is as simple as `golem-cli` using `golem-client` sending requests to Golem Services hosted in Docker container.
Therefore, you can see what's going on and troubleshoot things by inspecting docker containers.

```


+-----------------------+         +-----------------------+
|                       |         |                       |
|  Use golem-cli        |  --->   |  Golem Services       |
|                       |         |  hosted in            |
|  commands             |         |  Docker container     |
|  (Send Requests)      |         |                       |
+-----------------------+         +-----------------------+

```


## Compiling Golem locally
Find details in the [contribution guide](CONTRIBUTING.md) about how to compile the Golem services locally.
