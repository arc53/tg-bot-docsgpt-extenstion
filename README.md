# Telegram DocsGPT extension

This repository contains the source code for a Telegram bot that leverages DocsGPT to provide intelligent responses to user queries. This bot is an extension for [DocsGPT](https://www.docsgpt.cloud/).

## Features
- Responds to user queries with intelligent answers using DocsGPT.
- Maintains conversation history for context-aware responses.
- Easily deployable using Docker.

## Prerequisites
Before you begin, ensure you have met the following requirements:
- You have registered a bot with [BotFather](https://core.telegram.org/bots#botfather) on Telegram and obtained a `TOKEN`.
- You have created an API key on [DocsGPT](https://www.docsgpt.cloud/) to access your AI's data and prompt.

## Installation

### Using Python

1. Clone the repository:
    ```bash
    git clone https://github.com/arc53/tg-bot-docsgpt-extenstion.git
    cd tg-bot-docsgpt-extenstion
    ```

2. Set up a virtual environment:
    ```bash
    python3 -m venv venv
    ```
    - On macOS and Linux:
      ```bash
      source venv/bin/activate
      ```
    - On Windows:
      ```bash
      .\venv\Scripts\activate
      ```

3. Install dependencies:
    ```bash
    pip install -r requirements.txt
    ```

4. Create a `.env` file in the project directory and add your environment variables:
    ```plaintext
    TELEGRAM_BOT_TOKEN=<your-telegram-bot-token>
    API_KEY=<your-api-key>
    ```

5. Run the bot:
    ```bash
    python bot.py
    ```

### Using Docker

1. Clone the repository:
    ```bash
    git clone https://github.com/arc53/tg-bot-docsgpt-extenstion.git
    cd tg-bot-docsgpt-extenstion
    ```

2. Build the Docker image:
    ```bash
    docker build -t telegram-gpt-bot .
    ```

3. Create a `.env` file in the project directory and add your environment variables:
    ```plaintext
    TELEGRAM_BOT_TOKEN=<your-telegram-bot-token>
    API_KEY=<your-api-key>
    ```

4. Run the Docker container:
    ```bash
    docker run --env-file .env telegram-gpt-bot
    ```

## Usage

### Telegram Commands
- `/start` - Initiates the conversation with the bot.
- `/help` - Provides help information.

### General Conversation
Simply type any message, and the bot will respond with an intelligent answer based on the context of the conversation maintained in `context.chat_data`.

## File Description
- `bot.py`: The main script for running the bot.
- `requirements.txt`: Python dependencies required by the bot.
- `Dockerfile`: Instructions to build the Docker image.
- `.env`: File containing environment variables (not included, must be created).

## License
This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## Acknowledgments
- [DocsGPT](https://www.docsgpt.cloud/)
- [DocsGPT Github](https://github.com/arc53/docsgpt)
- [Telegram Bot API](https://core.telegram.org/bots/api)
- [Python Telegram Bot](https://python-telegram-bot.readthedocs.io/)