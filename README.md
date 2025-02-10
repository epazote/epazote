[![Test & Build](https://github.com/epazote/epazote/actions/workflows/build.yml/badge.svg)](https://github.com/epazote/epazote/actions/workflows/build.yml)
[![codecov](https://codecov.io/gh/epazote/epazote/graph/badge.svg?token=ztiGQV2sTm)](https://codecov.io/gh/epazote/epazote)

# Epazote 🌿
Automated HTTP (microservices) supervisor

## How it works
In its basic way of operation, **Epazote** periodically checks the services endpoints
"[URLs](https://en.wikipedia.org/wiki/Uniform_Resource_Locator)"
by doing an [HTTP GET Request](https://en.wikipedia.org/wiki/Hypertext_Transfer_Protocol#Request_methods),
based on the response [Status code](https://en.wikipedia.org/wiki/List_of_HTTP_status_codes),
[Headers](https://en.wikipedia.org/wiki/List_of_HTTP_header_fields) or
either the
[body](https://en.wikipedia.org/wiki/HTTP_message_body), it executes a command.

In most scenarios, is desired to apply a command directly to the application in
cause, like a signal (``kill -HUP``), or either a restart (``sv restart app``),
therefore in this case **Epazote** and the application should be running on the
same server.


# How to use it
First you need to install **Epazote**:

    cargo install epazote

Or download the latest release from the [releases](https://github.com/epazote/epazote/releases)


**Epazote** was designed with simplicity in mind, as an easy tool for
[DevOps](https://en.wikipedia.org/wiki/DevOps) and as a complement to
infrastructure orchestration tools like [Ansible](http://www.ansible.com/) and
[SaltStack](http://saltstack.com/), because of this [YAML](http://www.yaml.org/)
is used for the configuration files, avoiding with this, the learn of a new
language or syntax and simplifying the setup.

## Basic example

```yaml
services:
    myip:
        url: https://www.myip.country
        every: 5m
        expect:
            status: 200
            ssl:
                hours: 72
            if_not:
                cmd: echo -n "myip.country down"
```

To supervise ``myip.country`` you would run (basic.yml is a file containing the above code):

    $ epazote -c /path/to/yaml/file/basic.yml -v | jq

> -v is for debugging, will print all output to standard output.

This basic setup will supervise every 5 minutes the service with name
``myip``, it will do an HTTP GET to ``https://www.myip.country`` and will expect
an ``200 Status code`` if not,  it will ``echo -n "myip.country down"``

The ``ssl: hours: 72`` means to send an alert if the certificate is about to
expire in the next 72 hours.

## The configuration file

The configuration file ([YAML formatted](https://en.wikipedia.org/wiki/YAML))
consists of two parts, a **config** and a **services** (Key-value pairs).

## The config section

The **config** section is composed of:

    - scan (Paths used to find the file 'epazote.yml')

Example:

```yaml
config:
    scan:
        paths:
            - /arena/home/sites
            - /home/apps
        minutes: 5
```

### config - scan

Paths to scan every N ``seconds``, ``minutes`` or ``hours``, a search for
services specified in a file call ``epazote.yml`` is made.

The **scan** setting is optional however is very useful when doing Continues
Deployments. for example if your code is automatically uploaded to the
directory ``/arena/home/sites/application_1`` and your scan paths contain
``/arena/home/sites``, you could simple upload on your application directory a
file named ``epazote.yml`` with the service rules, thus achieving the deployment
of your application and the supervising at the same time.

## The services section

Services are the main functionality of **Epazote**, is where the URL's and the
rules based on the response are defined, since options vary from service to
service, an example could help better to understand the setup:

```yaml
services:
    my service 1:
        url: http://myservice.domain.tld/_healthcheck_
        timeout: 5
        seconds: 60
        log: http://monitor.domain.tld
        expect:
            status: 200
            header:
                content-type: application/json
            body: find this string on my site
            if_not:
                cmd: sv restart /services/my_service_1
        if_status:
            500:
                cmd: reboot
            404:
                cmd: sv restart /services/cache
        if_header:
            x-amqp-kapputt:
                cmd: restart abc
            x-db-kapputt:
                cmd: svc restart /services/db

    # do nothing
    other service:
        url: https://self-signed.ssl.tld/ping
        header:
            Origin: http://localhost
            Accept-Encoding: gzip
        insecure: true
        minutes: 3

    redirect service:
        url: http://test.domain.tld/
        follow: yes
        hour: 1
        expect:
            status: 302
            if_not:
                cmd: service restart abc

    salt-master:
        test: pgrep -f salt
        every: 5m
        expect:
            status: 0
            if_not:
                cmd: service restart salt_master
```

### services - name of service (string)
An unique string that identifies your service, in the above example, there are 3
services named:
 - my service 1
 - other service
 - redirect service

### services - url (string)
URL of the service to supervise

### services - follow (boolean true/false)
By default if a [302 Status code](https://en.wikipedia.org/wiki/HTTP_302) is
received, **Epazote** will not follow it, if you would like to follow all
redirects, this setting must be set to **true**.

### services - insecure (boolean true/false)
This option explicitly allows **Epazote** to perform "insecure" SSL connections.
It will disable the certificate verification.

### services - stop (int)
Defines the number or times the ``cmd`` will be executed, by default the ``cmd``
is executed only once, with the intention to avoid indefinitely loops. If value
is set to ``-1`` the ``cmd`` never stops. defaults to 0, ``stop 2`` will execute
"0, 1, 2" (3 times) the ``cmd``.

### services - timeout in seconds (int)
Timeout specifies a time limit for the HTTP requests, A value of zero means no
timeout, defaults to 5 seconds.

### services - retry_limit (int)
Specifies the number of times to retry an request, defaults to 3.

### services - retry_interval (int)
Specifies the time between attempts in milliseconds. The default value is 500 (0.5 seconds).

### services - read_limit (int)
Read only ``N`` number of bytes instead of the full
body. This helps to make a more "complete" request and
avoid getting an HTTP status code [408 when testing aws ELB](http://docs.aws.amazon.com/ElasticLoadBalancing/latest/DeveloperGuide/ts-el b-error-message.html#ts-elb-errorcodes-http408).

### services - seconds, minutes, hours
How often to check the service, the options are: (Only one should be used)
 - seconds N
 - minutes N
 - hours N

``N`` should be an integer.

### services - log (URL)
An URL to post all events, default disabled.

### services - expect
The ``expect`` block options are:
- status (int)
- header (key, value)
- body   (regular expression)
- if_not (Action block)

### services - expect - status
An Integer representing the expected [HTTP Status Code](https://en.wikipedia.org/wiki/List_of_HTTP_status_codes)

### services - expect - header (start_with match)
A key-value map of expected headers, it can be only one or more.

The headers will be considered valid if they starts with the required value,
for example if you want to check for ``Content-type: application/json; charset=utf-8``
you can simple do something like:

```yaml
    header:
        Content-Type: application/json
```

This helps to simplify the matching and useful in cases where the headers
changes, for example: ``Content-Range: bytes 100-64656926/64656927`` can be
matched with:

```yaml
    header:
        Content-Range: bytes
```

### services - expect - body
A [regular expression](https://en.wikipedia.org/wiki/Regular_expression) used
to match a string on the body of the site, use full in cases you want to ensure
that the content delivered is always the same or keeps a pattern.

### services - expect (How it works)
The ``expect`` logic tries to implement a
[if-else](https://en.wikipedia.org/wiki/if_else) logic ``status``, ``header``,
``body`` are the **if** and the ``if_not`` block becomes the **else**.

    if
        status
        header
        body
    else:
        if_not

In must cases only one option is required, check on the above example for the service named "redirect service".

In case that more than one option is used, this is the order in how they are evaluated, no meter how they where introduced on the configuration file:

    1. body
    2. status
    3. header

The reason for this order is related to performance, at the end we want to
monitor/supervise the services in an efficient way avoiding to waste extra
resources, in must cases only the HTTP Headers are enough to take an action,
therefore we don't need to read the full body page, because of this if no
``body`` is defined, **Epazote** will only read the Headers saving with this
time and process time.

### services - expect - if_not
``if_not`` is a block with an action of what to do it we don't get what we where
expecting (``expect``). See services - Actions

### services - if_status  & if_header
There maybe cases in where third-party dependencies are down and because of this
your application could not be working properly, for this cases the ``if_status``
and ``if_header`` could be useful.

For example if the database is your application could start responding an status
code 500 or either a custom header and based on does values take execute an
action:

The format for ``if_status`` is a key-pair where key is an int representing an
HTTP status code, and the value an Action option

The format for ``if_header`` is a key-pair where key is a string of something
you could relate/match and has in other if_X conditions, value is an Action.

This are the only ``if's`` and the order of execution:
 1. if_status
 2. if_header
 3. if_not

This means that if a service uses ``if_status`` and ``if_not``, it will
evaluate first the ``if_status`` and execute an Action if required, in case
an ``if_status`` and ``if_header`` are set, same applies, first is evaluated
``if_status``, then ``if_header`` and last ``if_not``.

## services - Actions
An Action has five options:
 - cmd
 - notify
 - msg
 - emoji
 - http

They can be used all together, only one or either none.

### services - Actions - cmd (string)
``cmd`` Contains the command to be executed.

### services - Actions - notify (string)
``notify`` Should contain ``yes``, the email email address or addresses (space separated)
of the recipients that will be notified when the action is executed.

If the string is ``yes`` the global recipients will be used.

### services - Actions - msg (list)
```yaml
msg:
 - send this if exit 0 (all OK)
 - send this if exit 1 (something is wrong)
```
Based on the exit status either msg[0] or msg[1] is used,

### services - Actions - emoji (list)
``emoji`` [Unicode](https://en.wikipedia.org/wiki/Unicode) characters
to be used in the subject, example:
```yaml
emoji:
  - 1F600
  - 1F621
```
If services are OK they will use the first ``1F600`` if not they will
use ``1F621``, if set to ``0`` no emoji will be used. The idea behind using
[unicode/emoji](https://en.wikipedia.org/wiki/Emoticons_(Unicode_block))
is to cough attention faster and not just ignore the email thinking is spam.

### service - Actions - http (list(key, value))
A custom URL to GET/POST depending on the exit status, example:
```yaml
http:
  - url: "https://api.hipchat.com/v1/rooms/message?auth_token=your_token&room_id=7&from=Alerts&message=service+OK+_name_+_because_"
  - url: "https://api.hipchat.com/"
    header:
      Content-Type: application/x-www-form-urlencoded
    data: |
     room_id=10&from=Alerts&message=_name_+exit+code+_exit_
    method: POST
```
When a service fails or returns an exit 1 the second url
``https://api.hipchat.com/`` with method ``POST`` and the custom ``data``
will be used, notice that all the ocurances on the data that are within an
``_(key)_`` will be replaced with the corresponding value, in this case:

     room_id=10&from=Alerts&message=_name_+exit+code+_exit_

will be replaced with:

     room_id=10&from=Alerts&message=SERVICE NAME+exit+code+0

When recovery the first url will be used, in this case will be a GET instead of a post, so:

    https://api.hipchat.com/v1/rooms/message?auth_token=your_token&room_id=7&from=Alerts&message=service+OK+_name_+_because_

becomes:

    https://api.hipchat.com/v1/rooms/message?auth_token=your_token&room_id=7&from=Alerts&message=service+OK+SERVICE+NAME+STATUS+200

> notice that the _name_, _exit_, _because_ are been replaced with the values of name, exit, because of the service.


## services - Test
**Epazote** It is mainly used for HTTP services, for supervising other
applications that don't listen or accept HTTP connections, like a database,
cache engine, etc. There are tools like
[daemontools](https://cr.yp.to/daemontools.html),
[runit](http://smarden.org/runit/) as already mentioned, even so, **Epazote**
can eventually be used to execute an action based on the exit of a command
for example:

```yaml
    salt-master:
        test: pgrep -f salt
        if_not:
            cmd: service restart salt_master
            notify: operations@domain.tld
```

In this case: ``test: pgrep -f salt`` will execute the ``cmd`` on the ``if_not``
block in case the exit code is > 0, from the ``pgrep`` man page:

```txt
EXIT STATUS
     The pgrep and pkill utilities return one of the following values upon exit:

          0       One or more processes were matched.
          1       No processes were matched.
          2       Invalid options were specified on the command line.
          3       An internal error occurred.
```


## Extra setup
*green dots give some comfort* -- Because of this when using the ``log``
option an extra service could be configure as a receiver for all the post
that **Epazote** produce and based on the data obtained create a custom
dashboard, something similar to: https://status.cloud.google.com/ or
http://status.aws.amazon.com/

# Issues
Please report any problem, bug, here: https://github.com/nbari/epazote/issues
