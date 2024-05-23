import logging
import httpx
import re
import os
import json
from telegram import ForceReply, Update
from telegram.ext import Application, CommandHandler, ContextTypes, MessageHandler, filters
from telegram.constants import ParseMode

# Enable logging
logging.basicConfig(
    format="%(asctime)s - %(name)s - %(levelname)s - %(message)s", level=logging.INFO)
# set higher logging level for httpx to avoid all GET and POST requests being logged
logging.getLogger("httpx").setLevel(logging.WARNING)
logger = logging.getLogger(__name__)

API_BASE = os.getenv("API_BASE", "https://gptcloud.arc53.com")
API_URL =  API_BASE + "/api/answer"
API_KEY = os.getenv("API_KEY")
TOKEN = os.getenv("TELEGRAM_BOT_TOKEN")


async def start(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Send a message when the command /start is issued."""
    user = update.effective_user
    await update.message.reply_html(
        rf"Hi {user.mention_html()}!",
        reply_markup=ForceReply(selective=True),
    )


async def help_command(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    """Send a message when the command /help is issued."""
    await update.message.reply_text("Help!")


async def generate_answer(question: str, messages: list, conversation_id: str | None) -> dict:
    """Generates an answer using the external API."""
    payload = {
        "question": question,
        "api_key": API_KEY,
        "history": messages,
        "conversation_id": conversation_id
    }
    headers = {
        "Content-Type": "application/json; charset=utf-8"
    }
    timeout = 60.0
    async with httpx.AsyncClient() as client:
        response = await client.post(API_URL, json=payload, headers=headers, timeout=timeout)

        if response.status_code == 200:
            data = response.json()
            conversation_id = data.get("conversation_id")
            answer = data.get("answer", "Sorry, I couldn't find an answer.")
            return {"answer": answer, "conversation_id": conversation_id}
        else:
            return {"answer": "Sorry, I couldn't find an answer.", "conversation_id": None}


def escape_markdown(text: str) -> str:
    """Helper function to escape telegram markup symbols."""
    escape_chars = '\*_\[\]()~>#+-=|{}.!'
    return re.sub(r'([%s])' % escape_chars, r'\\\1', text)


async def echo(update: Update, context: ContextTypes.DEFAULT_TYPE) -> None:
    # Store the conversation history in the context
    if "conversation_history" not in context.chat_data:
        context.chat_data["conversation_history"] = []
    if "conversation_id" not in context.chat_data:
        context.chat_data["conversation_id"] = None

    context.chat_data["conversation_history"].append({"prompt": update.message.text})
    
    # Generate answer based on current message and conversation history
    response_doc = await generate_answer(update.message.text, 
      context.chat_data["conversation_history"], 
      context.chat_data["conversation_id"])
    
    answer = response_doc["answer"]
    conversation_id = response_doc["conversation_id"]

    # answer is in markdown format
    answer = escape_markdown(answer)

    await update.message.reply_text(answer, parse_mode=ParseMode.MARKDOWN_V2)

    context.chat_data["conversation_history"][-1]["response"] = answer
    context.chat_data["conversation_id"] = conversation_id

    context.chat_data["conversation_history"] = context.chat_data["conversation_history"][-10:]


def main() -> None:
    """Start the bot."""
    # Create the Application and pass it your bot's token.
    application = Application.builder().token(TOKEN).build()

    # on different commands - answer in Telegram
    application.add_handler(CommandHandler("start", start))
    application.add_handler(CommandHandler("help", help_command))

    # on non command i.e message - echo the message on Telegram
    application.add_handler(MessageHandler(filters.TEXT & ~filters.COMMAND, echo))

    # Run the bot until the user presses Ctrl-C
    application.run_polling(allowed_updates=Update.ALL_TYPES)


if __name__ == "__main__":
    main()