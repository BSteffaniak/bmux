# BMUX connections administration

Connection policy shared by the pre-admission entry points and connections
command handlers. Legacy transports without admission proof must refuse ingress
before opening a listener or copying bytes.
