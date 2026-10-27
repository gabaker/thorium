# Creating a Tool from Image Settings

Images can no longer be created (see [Legacy Images and Pipelines](./images.md)); an Image tool holds the same settings.

To add a new tool, you must tell Thorium how to run it via the image configuration settings of its Image spec. This runtime
configuration may seem complicated, but has been designed to minimize or eliminate the need to customize your tool
to work within Thorium. You tell Thorium how to run your tool and where your tool writes its outputs/results and
Thorium can then handle executing your image within an analysis pipeline. Your tool does not need to know how to
communicate with the Thorium API. Because of this functionality, any command line (CLI) tool that can run in a
container or on bare metal can be added as a new Image tool without any customization.

Create a tool from a spec file that holds these settings with `thorctl tools create --spec <SPEC>`
(see [Tools and Tool Instances](./tools.md#an-image-tool)). `thorctl images import` fails because it
writes images. To bring a set of tools into an instance, import a
[toolbox](./toolbox.md): it imports tools, and converts a legacy image toolbox to tools as it reads it.

If you want to know more about the available image configuration options, you can go to the next section that explains
how to [configure an images](./configuring_images.md). This section covers the required image configuration settings as
well as the more advanced optional settings.
