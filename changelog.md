Changelog
=========

## 2.1.0
- fixed a bug on report.go https://github.com/epazote/epazote/issues/3

## 2.0.0
- SSL Alert before certificate expires ``ssl``
- Threshold healthy/unhealthy
- When debugging cookies are properly parsed
- Cleaned up the tests

## 1.5.2
- fallback to ``sh`` if no ``$SHELL`` found.

## 1.5.1
- use ``$SHELL -c`` instead of ``sh -c`` in to allow piped commands.
- ``-v`` print git commit hash only if available.

## 1.5.0
- ``test`` using ``sh -c 'cmd'`` to allow piped commands.
- Implemented ``read_limit``, for reading only ``N`` number of bytes instead of the full body. This helps to make a more "complete" request and avoid getting an HTTP status code [408 when testing aws ELB](http://docs.aws.amazon.com/ElasticLoadBalancing/latest/DeveloperGuide/ts-elb-error-message.html#ts-elb-errorcodes-http408).
- Implemented ``Timeout``, ``KeepAlive`` and ``TLSHandshakeTimeout`` default values in ``HTTPGet``.
- ``-v`` prints version + git commit hash.

## 1.4.0
- Increased debugging, response headers included.
- Implement ``http`` in action, An URL to "GET/POST" in case service is up/down, for example 'hipchat/mailgun'.
- kill -USR1 shows cleaner info.
- Implement ``Retry`` default to 3, with 0.5 second (500 milliseconds) interval.
- Fix ``Emoji`` and ``msg`` implementation to behave like a list.
- Implement timestamp ``when`` RFC3339.

## 1.3.0
- ``Insecure`` feature to ignore SSL / self signed certificates.
- ``Stop`` establish a limit on how many times to retry a cmd, ``-1`` loops for ever.
- ``Emoji`` support per action, add/remove icons on email subject.

## 1.2.0
- Improve expect/header match.
- Fix service notification to avoid spamming recipients.

## 1.1.0
- Added -d debug flag.
- Added ``Follow`` setting to avoid/allow following redirects.
