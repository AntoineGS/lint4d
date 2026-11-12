unit DfmHandler;

interface

uses
  System.Classes, Vcl.Forms, Vcl.StdCtrls;

type
  TTestForm = class(TForm)
  private
    Button1: TButton;
    ButtonNeverUsed: TButton;
    procedure PrivateHandler(Sender: TObject);
  end;

var
  TestForm: TTestForm;

implementation

{$R *.dfm}

procedure TTestForm.PrivateHandler(Sender: TObject);
begin
end;

end.
